package senclaw

// The HTTP host — the parts the daemon depends on. This is exactly what the
// daemon does to an app: waits for healthPath, POSTs JSON-RPC at mcp.path, then
// proxies the UI.

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func get(t *testing.T, srv *httptest.Server, path string) (int, string) {
	t.Helper()
	resp, err := srv.Client().Get(srv.URL + path)
	if err != nil {
		t.Fatalf("GET %s: %v", path, err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(body)
}

func TestHealthMCPAndStaticAllAnswerOnOneHandler(t *testing.T) {
	web := t.TempDir()
	if err := os.WriteFile(filepath.Join(web, "index.html"), []byte("<h1>ui</h1>"), 0o644); err != nil {
		t.Fatal(err)
	}

	srv := httptest.NewServer(Handler(Config{
		Routes: map[string]http.Handler{
			"GET /api/status": JSONHandler(func(*http.Request) (any, error) {
				return map[string]any{"ok": true, "detail": "from the app"}, nil
			}),
			"POST /api/thing": JSONHandler(func(r *http.Request) (any, error) {
				var body map[string]any
				if err := Bind(r, &body); err != nil {
					return nil, err
				}
				return map[string]any{"got": body}, nil
			}),
			"GET /files/*": JSONHandler(func(r *http.Request) (any, error) {
				return map[string]any{"path": r.URL.Path}, nil
			}),
		},
		HealthPath: "/api/status",
		MCPPath:    "/api/mcp/sse",
		MCP:        buildMCP(),
		StaticDir:  web,
	}))
	defer srv.Close()

	// The app's own handler at the health path wins over the built-in one: an
	// app that reports its real state must not be overwritten with {ok:true}.
	status, body := get(t, srv, "/api/status")
	if status != 200 || !strings.Contains(body, "from the app") {
		t.Fatalf("health = %d %s", status, body)
	}

	resp, err := srv.Client().Post(srv.URL+"/api/mcp/sse", "application/json",
		strings.NewReader(`{"jsonrpc":"2.0","id":1,"method":"tools/list"}`))
	if err != nil {
		t.Fatal(err)
	}
	var rpc struct {
		Result struct {
			Tools []struct {
				Name string `json:"name"`
			} `json:"tools"`
		} `json:"result"`
	}
	_ = json.NewDecoder(resp.Body).Decode(&rpc)
	resp.Body.Close()
	if len(rpc.Result.Tools) != 2 || rpc.Result.Tools[0].Name != "demo_echo" {
		t.Fatalf("tools = %+v", rpc.Result.Tools)
	}

	resp, err = srv.Client().Post(srv.URL+"/api/thing", "application/json", strings.NewReader(`{"x":1}`))
	if err != nil {
		t.Fatal(err)
	}
	body2, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if !strings.Contains(string(body2), `"x":1`) {
		t.Fatalf("POST body = %s", body2)
	}

	if _, b := get(t, srv, "/files/deep/one.txt"); !strings.Contains(b, "/files/deep/one.txt") {
		t.Fatalf("prefix route = %s", b)
	}

	if _, b := get(t, srv, "/"); !strings.Contains(b, "ui") {
		t.Fatalf("index = %s", b)
	}
	// An unknown path is a client-side route, not a 404 — SPAs depend on it.
	if _, b := get(t, srv, "/some/deep/route"); !strings.Contains(b, "ui") {
		t.Fatalf("SPA fallback = %s", b)
	}
}

func TestTheBuiltInHealthEndpointAnswersWhenTheAppRegistersNone(t *testing.T) {
	srv := httptest.NewServer(Handler(Config{}))
	defer srv.Close()
	if status, body := get(t, srv, "/health"); status != 200 || !strings.Contains(body, `"ok":true`) {
		t.Fatalf("health = %d %s", status, body)
	}
	if status, _ := get(t, srv, "/nope"); status != 404 {
		t.Fatalf("unknown path = %d", status)
	}
}

func TestAStaticPathCannotEscapeTheWebRoot(t *testing.T) {
	root := t.TempDir()
	web := filepath.Join(root, "web")
	if err := os.Mkdir(web, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(web, "index.html"), []byte("ok"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "secret.txt"), []byte("SECRET"), 0o644); err != nil {
		t.Fatal(err)
	}

	srv := httptest.NewServer(Handler(Config{StaticDir: web}))
	defer srv.Close()

	for _, p := range []string{"/../secret.txt", "/..%2fsecret.txt", "/web/../../secret.txt", "/%2e%2e/secret.txt"} {
		req, err := http.NewRequest(http.MethodGet, srv.URL+p, nil)
		if err != nil {
			t.Fatalf("%s: %v", p, err)
		}
		resp, err := srv.Client().Do(req)
		if err != nil {
			continue // a client-side rejection is the right answer too
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if strings.Contains(string(body), "SECRET") {
			t.Fatalf("%s reached outside the web root", p)
		}
	}
}

func TestJSONHandlerMapsErrorsToStatuses(t *testing.T) {
	srv := httptest.NewServer(Handler(Config{Routes: map[string]http.Handler{
		"GET /api/bad": JSONHandler(func(*http.Request) (any, error) {
			return nil, Statusf(http.StatusBadRequest, "id is required")
		}),
		"GET /api/broken": JSONHandler(func(*http.Request) (any, error) {
			return nil, errf("the database is gone")
		}),
	}}))
	defer srv.Close()

	if status, body := get(t, srv, "/api/bad"); status != 400 || !strings.Contains(body, "id is required") {
		t.Fatalf("bad = %d %s", status, body)
	}
	// An unclassified failure is a 500 the caller can read, not a panic.
	if status, body := get(t, srv, "/api/broken"); status != 500 || !strings.Contains(body, "database is gone") {
		t.Fatalf("broken = %d %s", status, body)
	}
}

func TestMergeRoutesLetsTheAppOverrideAMergedRoute(t *testing.T) {
	first := map[string]http.Handler{"GET /a": JSONHandler(func(*http.Request) (any, error) { return "first", nil })}
	second := map[string]http.Handler{"GET /a": JSONHandler(func(*http.Request) (any, error) { return "second", nil })}
	srv := httptest.NewServer(Handler(Config{Routes: MergeRoutes(first, second)}))
	defer srv.Close()
	if _, body := get(t, srv, "/a"); !strings.Contains(body, "second") {
		t.Fatalf("body = %s", body)
	}
}
