// {{title_name}} — a SenClaw Space App in one file, standard library only.
//
// What the daemon does with this, in order:
//
//  1. Reads senclaw-manifest.json. `runtime.runner` is `binary`, so it launches
//     `./{{id}}` — the binary scripts/pack.sh builds. **A Go app has no install
//     step**: `runtime.install` runs for the node and python runners only, so
//     whatever `start` names must already be runnable.
//  2. `runtime.mode: "session"` — nothing starts at boot. The app starts when
//     the user opens it or an agent calls one of the tools below, and stops 60
//     seconds after the last request.
//
// The tools stay in every agent's roster while this is stopped: the tool list is
// cached and the MCP URL points at the daemon's proxy, which starts the app
// before forwarding the call.
//
// Run it by hand during development:
//
//	SENCLAW_SPACE_APP_ID={{id}} PORT={{port}} go run .
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"time"
)

var (
	appID   = envOr("SENCLAW_SPACE_APP_ID", "{{id}}")
	baseURL = strings.TrimRight(envOr("SENCLAW_BASE_URL", "http://127.0.0.1:18788"), "/")
	// This app's access token, injected by the daemon. Sent on every call to
	// it: under the default strict mode a tokenless call to an app's data
	// routes is refused, and a token presented against another app's id is
	// refused always.
	appToken   = os.Getenv("SENCLAW_TOKEN_ACCESS_APP")
	apiVersion = envOr("SENCLAW_API_VERSION", "{{api_version}}")
	startedAt  = time.Now()
)

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

// ---------------------------------------------------------------------------
// Talking to the daemon
// ---------------------------------------------------------------------------

// missingOK is only for routes where 404 genuinely means "not set" — the config
// KV and nothing else. Treating 404 as nil everywhere turns a bridge that has
// moved (an older daemon, a proxy path change, a typo in the app id) into an
// empty *successful* summary the agent cannot tell from a real one.
func daemon(method, suffix string, body any, missingOK bool) (map[string]any, error) {
	var payload io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		payload = bytes.NewReader(raw)
	}
	req, err := http.NewRequest(method, baseURL+"/api/space/apps/"+appID+suffix, payload)
	if err != nil {
		return nil, err
	}
	req.Header.Set("x-senclaw-api-version", apiVersion)
	if appToken != "" {
		req.Header.Set("x-senclaw-app-token", appToken)
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusNotFound && missingOK {
		return nil, nil
	}
	var out map[string]any
	raw, _ := io.ReadAll(resp.Body)
	_ = json.Unmarshal(raw, &out)
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("%s %s → HTTP %d: %s", method, suffix, resp.StatusCode, raw)
	}
	return out, nil
}

// llm asks the daemon's model. The app never holds a provider API key.
func llm(prompt string, maxTokens int) (string, error) {
	out, err := daemon("POST", "/bridge", map[string]any{
		// The wire field is "action", not "capability". The daemon's request
		// struct requires it, and a body without it is rejected by the JSON
		// extractor with a 422 before any handler runs.
		"action": "llm.request",
		// Only these fields are honoured — temperature and friends are not part
		// of the bridge contract and are silently dropped.
		"payload": map[string]any{"prompt": prompt, "maxTokens": maxTokens},
	}, false)
	if err != nil {
		return "", err
	}
	// A failed completion comes back as HTTP **200** with status "error".
	// Checking only the HTTP status turns a provider outage into a successful
	// empty summary, which the agent has no way to notice.
	if out["status"] == "error" {
		msg, _ := out["message"].(string)
		if msg == "" {
			msg = "model trả về lỗi không rõ"
		}
		return "", fmt.Errorf("%s", msg)
	}
	if out["finish"] == "length" {
		return "", fmt.Errorf("câu trả lời bị cắt ở maxTokens — chia nhỏ công việc ra")
	}
	if s, ok := out["text"].(string); ok {
		return s, nil
	}
	s, _ := out["content"].(string)
	return s, nil
}

// The config KV, shared with the app's own settings UI.
func getConfig(key string) (any, error) {
	out, err := daemon("GET", "/config/"+key, nil, true)
	if err != nil || out == nil {
		return nil, err
	}
	return out["value"], nil
}

func setConfig(key string, value any) error {
	_, err := daemon("PUT", "/config/"+key, map[string]any{"value": value}, false)
	return err
}

// ---------------------------------------------------------------------------
// MCP: what agents can do with this app.
//
// The description is the only thing the model sees when choosing a tool — say
// what it does *and when to reach for it*. An error that reads like a sentence
// tells the agent what to do differently; a transport error tells it nothing.
// ---------------------------------------------------------------------------

type tool struct {
	description string
	schema      map[string]any
	run         func(args map[string]any) (any, error)
}

var tools = map[string]tool{
	"{{snake_name}}_status": {
		description: "Xem {{title_name}} đang chạy ra sao: thời gian hoạt động và phiên bản Go. " +
			"Dùng khi người dùng hỏi app còn sống không.",
		schema: map[string]any{"type": "object", "properties": map[string]any{}},
		run: func(map[string]any) (any, error) {
			return map[string]any{
				"app":        appID,
				"go":         runtime.Version(),
				"platform":   runtime.GOOS + "/" + runtime.GOARCH,
				"uptimeSecs": int(time.Since(startedAt).Seconds()),
			}, nil
		},
	},
	"{{snake_name}}_summarise": {
		description: "Tóm tắt một đoạn văn bản thành đúng ba câu. " +
			"Dùng khi người dùng đưa một đoạn dài và muốn ý chính.",
		schema: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"text": map[string]any{"type": "string", "description": "Đoạn văn bản cần tóm tắt."},
			},
			"required": []string{"text"},
		},
		run: func(args map[string]any) (any, error) {
			text, _ := args["text"].(string)
			if strings.TrimSpace(text) == "" {
				return errorContent("`text` đang rỗng — truyền đoạn văn bản cần tóm tắt."), nil
			}
			return llm("Tóm tắt đoạn sau thành đúng ba câu:\n\n"+text, 600)
		},
	},
}

func errorContent(msg string) map[string]any {
	return map[string]any{
		"isError": true,
		"content": []any{map[string]any{"type": "text", "text": msg}},
	}
}

func handleMCP(req map[string]any) map[string]any {
	id := req["id"]
	ok := func(result any) map[string]any {
		return map[string]any{"jsonrpc": "2.0", "id": id, "result": result}
	}
	fail := func(code int, msg string) map[string]any {
		return map[string]any{"jsonrpc": "2.0", "id": id,
			"error": map[string]any{"code": code, "message": msg}}
	}

	method, _ := req["method"].(string)
	switch method {
	case "initialize":
		return ok(map[string]any{
			"protocolVersion": "2024-11-05",
			"capabilities":    map[string]any{"tools": map[string]any{}},
			"serverInfo":      map[string]any{"name": "{{mcp_name}}", "version": "0.1.0"},
		})
	// SenClaw sends this as a request with an id, not a notification, and
	// ignores the reply — but erroring on it looks like a broken server.
	case "ping", "initialized", "notifications/initialized":
		return ok(map[string]any{})
	case "tools/list":
		list := make([]any, 0, len(tools))
		for name, t := range tools {
			list = append(list, map[string]any{
				"name": name, "description": t.description, "inputSchema": t.schema,
			})
		}
		return ok(map[string]any{"tools": list})
	case "tools/call":
		params, _ := req["params"].(map[string]any)
		name, _ := params["name"].(string)
		t, found := tools[name]
		if !found {
			names := make([]string, 0, len(tools))
			for n := range tools {
				names = append(names, n)
			}
			return fail(-32602, "không có tool tên "+name+" (đang có: "+strings.Join(names, ", ")+")")
		}
		args, _ := params["arguments"].(map[string]any)
		if args == nil {
			args = map[string]any{}
		}
		out, err := t.run(args)
		if err != nil {
			return ok(errorContent(err.Error()))
		}
		if m, isMap := out.(map[string]any); isMap {
			if _, hasContent := m["content"]; hasContent {
				return ok(m)
			}
		}
		text, isText := out.(string)
		if !isText {
			raw, _ := json.MarshalIndent(out, "", "  ")
			text = string(raw)
		}
		return ok(map[string]any{"content": []any{map[string]any{"type": "text", "text": text}}})
	default:
		return fail(-32601, "method not found: "+method)
	}
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

func writeJSON(w http.ResponseWriter, status int, body any) {
	raw, _ := json.Marshal(body)
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write(raw)
}

func main() {
	port := {{port}}
	if v, err := strconv.Atoi(os.Getenv("PORT")); err == nil && v > 0 {
		port = v
	}

	mux := http.NewServeMux()

	// runtime.healthPath. The daemon waits on this before it calls the app
	// started and polls it afterwards, so it must stay cheap and never block.
	mux.HandleFunc("/api/status", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]any{
			"ok": true, "app": appID,
			"uptimeSecs": int(time.Since(startedAt).Seconds()),
		})
	})

	mux.HandleFunc("/api/visit", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"error": "POST only"})
			return
		}
		current, err := getConfig("visits")
		if err != nil {
			writeJSON(w, http.StatusBadGateway, map[string]any{"error": err.Error()})
			return
		}
		visits := 0
		if f, isNum := current.(float64); isNum {
			visits = int(f)
		}
		visits++
		if err := setConfig("visits", visits); err != nil {
			writeJSON(w, http.StatusBadGateway, map[string]any{"error": err.Error()})
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"visits": visits})
	})

	mux.HandleFunc("/api/mcp/sse", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			// The SSE half of the transport: the client opens it, this app has
			// nothing to push.
			w.Header().Set("Content-Type", "text/event-stream")
			w.WriteHeader(http.StatusOK)
			return
		}
		var req map[string]any
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]any{"jsonrpc": "2.0", "id": nil,
				"error": map[string]any{"code": -32700, "message": "parse error"}})
			return
		}
		writeJSON(w, http.StatusOK, handleMCP(req))
	})

	// Static UI, from the directory next to the binary so the packed app and
	// `go run .` behave the same.
	webDir := "web"
	if exe, err := os.Executable(); err == nil {
		if candidate := filepath.Join(filepath.Dir(exe), "web"); dirExists(candidate) {
			webDir = candidate
		}
	}
	mux.Handle("/", http.FileServer(http.Dir(webDir)))

	// Loopback by default. A Space App authenticates nothing of its own — the
	// daemon reaches it over 127.0.0.1 and the UI is same-origin — so binding
	// 0.0.0.0 hands the whole REST + MCP surface to anyone on the LAN. Set
	// SENCLAW_BIND_HOST=0.0.0.0 to opt in to that explicitly.
	host := envOr("SENCLAW_BIND_HOST", "127.0.0.1")
	addr := fmt.Sprintf("%s:%d", host, port)
	server := &http.Server{Addr: addr, Handler: mux}

	// A session app is stopped with SIGTERM and killed about two seconds later.
	// Close and flush; do not start new work.
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-stop
		log.Printf("[%s] shutting down", appID)
		_ = server.Close()
	}()

	log.Printf("[%s] listening on http://%s", appID, addr)
	if err := server.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		log.Fatal(err)
	}
}

func dirExists(p string) bool {
	info, err := os.Stat(p)
	return err == nil && info.IsDir()
}
