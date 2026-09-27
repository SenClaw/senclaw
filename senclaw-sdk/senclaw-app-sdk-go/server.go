package senclaw

// Serving a Go Space App: health, static UI, REST and MCP in one process.
//
// The daemon expects one HTTP server per app, on the port it hands out in PORT,
// answering:
//
//   - runtime.healthPath — anything 2xx. The daemon waits on this before it
//     considers the app started (30s budget), and the supervisor polls it.
//   - mcp.path — the app's MCP endpoint, JSON-RPC over HTTP POST.
//   - everything else — the app's own REST API and its UI, which the daemon
//     reverse-proxies at /api/space/apps/<id>/proxy/….
//
// net/http already does the hard part. What this adds is the four things a
// hand-rolled main gets wrong: binding loopback rather than 0.0.0.0, reading
// the port out of PORT, serving a single-page UI without letting a request
// climb out of the web root, and handling SIGTERM.
//
// That last one is worth reading before writing an app: a session app is
// stopped when it goes idle, and the daemon signals the process group with
// SIGTERM and SIGKILLs it two seconds later. Two seconds is plenty to flush,
// and nothing if you ignore the signal.

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"mime"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

// MaxBodyBytes caps a request body read by [Bind] and by the MCP endpoint.
const MaxBodyBytes = 32 << 20 // 32 MiB

// Config describes the app's HTTP surface.
type Config struct {
	// Routes maps "METHOD /path" to a handler, e.g. "GET /api/status". A path
	// ending in /* matches by prefix and the handler gets the full path.
	Routes map[string]http.Handler

	// HealthPath is runtime.healthPath. Registering your own route at the same
	// path takes precedence — an app that reports its real state must not be
	// overwritten with {"ok":true}. Defaults to /health.
	HealthPath string

	// StaticDir is the app's built web UI, served with an index.html fallback
	// for unknown paths so a client-side router works.
	StaticDir string

	// MCPPath is the manifest's mcp.path, and MCP is usually the [MCPServer].
	MCPPath string
	MCP     http.Handler

	// OnShutdown runs on SIGTERM/SIGINT, before the listener closes. Budget
	// about two seconds: flush and close, do not start new work.
	OnShutdown func(context.Context) error

	// DefaultPort is used when PORT is unset — running the app by hand.
	DefaultPort int

	// Log defaults to writing to stdout, which the daemon captures into the
	// app's log file.
	Log func(string)

	// Middleware wraps the whole handler, outermost first. Use it for request
	// logging or auth of your own; the daemon adds none.
	Middleware []func(http.Handler) http.Handler

	// RequireAppToken refuses any request that does not carry this app's access
	// token, closing the app's own API to everything except the daemon.
	//
	// An app's REST and MCP endpoints have no authentication of their own: the
	// port is open to every process on the machine, and the app id in a URL is
	// public. With this on, the only caller that gets through is the daemon —
	// its proxy stamps the token on every request it forwards (the UI iframe,
	// the app's own fetches, MCP tool calls), and a direct hit on the port by
	// anything else answers 401.
	//
	// Off by default, because turning it on breaks two real patterns that talk
	// to the port directly: a browser extension dialling ws://127.0.0.1:<port>,
	// and a developer's curl. Both are served by AuthSkipPaths.
	RequireAppToken bool

	// AuthSkipPaths are exempt from RequireAppToken, matched exactly or by a
	// trailing /* prefix. The health path is always exempt — the daemon's
	// health check is what decides the app started, and it runs before the app
	// is ever proxied to.
	AuthSkipPaths []string
}

// Handler builds the app's http.Handler without listening, which is what tests
// want: hand it to httptest.NewServer and exercise the real routing.
func Handler(cfg Config) http.Handler {
	healthPath := cfg.HealthPath
	if healthPath == "" {
		healthPath = "/health"
	}
	staticRoot := ""
	if cfg.StaticDir != "" {
		if abs, err := filepath.Abs(cfg.StaticDir); err == nil {
			staticRoot = abs
		}
	}

	type prefixRoute struct {
		method  string
		prefix  string
		handler http.Handler
	}
	exact := map[string]http.Handler{}
	var prefixes []prefixRoute
	for key, h := range cfg.Routes {
		method, pattern := splitRoute(key)
		if strings.HasSuffix(pattern, "/*") {
			prefixes = append(prefixes, prefixRoute{method, strings.TrimSuffix(pattern, "*"), h})
			continue
		}
		exact[method+" "+pattern] = h
	}

	var h http.Handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Clean the path ourselves: this handler sees every request, so there
		// is no mux to have done it, and `/..%2fsecret` arrives decoded.
		urlPath := path.Clean("/" + r.URL.Path)

		if cfg.MCP != nil && cfg.MCPPath != "" && urlPath == cfg.MCPPath {
			cfg.MCP.ServeHTTP(w, r)
			return
		}
		if route, ok := exact[r.Method+" "+urlPath]; ok {
			route.ServeHTTP(w, r)
			return
		}
		for _, p := range prefixes {
			if r.Method == p.method && strings.HasPrefix(urlPath, p.prefix) {
				p.handler.ServeHTTP(w, r)
				return
			}
		}
		// The built-in health endpoint comes after the routes, so an app that
		// registers its own handler there gets to answer with it.
		if urlPath == healthPath && (r.Method == http.MethodGet || r.Method == http.MethodHead) {
			JSON(w, http.StatusOK, map[string]any{"ok": true})
			return
		}
		if staticRoot != "" && (r.Method == http.MethodGet || r.Method == http.MethodHead) {
			if serveStatic(w, r, staticRoot, urlPath) {
				return
			}
		}
		JSON(w, http.StatusNotFound, map[string]any{"error": "not found", "path": urlPath})
	})

	if cfg.RequireAppToken {
		h = RequireAppToken(AppTokenFromEnv(), append([]string{healthPath}, cfg.AuthSkipPaths...))(h)
	}
	for i := len(cfg.Middleware) - 1; i >= 0; i-- {
		h = cfg.Middleware[i](h)
	}
	return h
}

// RequireAppToken is the middleware behind [Config.RequireAppToken], exported
// for apps that build their own handler chain.
//
// token is what the request must present in HeaderAppToken — normally
// [AppTokenFromEnv]. An empty token disables the check rather than locking the
// app out of itself: that is what an app running outside SenClaw sees, and
// answering 401 to every request including the daemon's health check would turn
// "no token issued" into "app permanently down".
func RequireAppToken(token string, skip []string) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		if strings.TrimSpace(token) == "" {
			return next
		}
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			urlPath := path.Clean("/" + r.URL.Path)
			for _, s := range skip {
				if s == "" {
					continue
				}
				if strings.HasSuffix(s, "/*") {
					if strings.HasPrefix(urlPath, strings.TrimSuffix(s, "*")) {
						next.ServeHTTP(w, r)
						return
					}
				} else if urlPath == s {
					next.ServeHTTP(w, r)
					return
				}
			}
			presented := strings.TrimSpace(r.Header.Get(HeaderAppToken))
			if presented == "" {
				presented = strings.TrimSpace(r.URL.Query().Get("app_token"))
			}
			// Constant time: a byte-by-byte compare on a secret leaks the
			// length of the matched prefix through timing.
			if subtle.ConstantTimeCompare([]byte(presented), []byte(token)) != 1 {
				JSON(w, http.StatusUnauthorized, map[string]any{
					"error": "this app only answers requests from the SenClaw daemon",
					"code":  "app_token_required",
				})
				return
			}
			next.ServeHTTP(w, r)
		})
	}
}

// Serve runs the app's HTTP server until the daemon stops it. It blocks, and
// returns nil on a clean shutdown.
func Serve(cfg Config) error {
	logf := cfg.Log
	if logf == nil {
		logf = func(msg string) { fmt.Println(msg) }
	}
	port, err := Port(cfg.DefaultPort)
	if err != nil {
		return err
	}
	addr := net.JoinHostPort(BindHost(), fmt.Sprint(port))
	srv := &http.Server{
		Addr:              addr,
		Handler:           Handler(cfg),
		ReadHeaderTimeout: 10 * time.Second,
	}

	// SIGTERM is what the daemon sends when it stops an idle session app, and
	// what it sends every app on its own shutdown. Two seconds later it is
	// SIGKILL, so anything unflushed at that point is lost.
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM, syscall.SIGINT)
	done := make(chan error, 1)

	go func() {
		sig := <-stop
		logf(fmt.Sprintf("[senclaw] %v — shutting down", sig))
		ctx, cancel := context.WithTimeout(context.Background(), 1500*time.Millisecond)
		defer cancel()
		if cfg.OnShutdown != nil {
			if err := cfg.OnShutdown(ctx); err != nil {
				logf("[senclaw] shutdown handler failed: " + err.Error())
			}
		}
		done <- srv.Shutdown(ctx)
	}()

	logf(fmt.Sprintf("[senclaw] listening on http://%s", addr))
	if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	select {
	case err := <-done:
		if err != nil && !errors.Is(err, context.DeadlineExceeded) {
			return err
		}
	case <-time.After(2 * time.Second):
	}
	return nil
}

func splitRoute(key string) (method, pattern string) {
	fields := strings.Fields(key)
	if len(fields) >= 2 {
		return strings.ToUpper(fields[0]), fields[1]
	}
	return http.MethodGet, key
}

// serveStatic answers from root, or reports false when it has nothing to say.
func serveStatic(w http.ResponseWriter, r *http.Request, root, urlPath string) bool {
	rel := strings.TrimPrefix(urlPath, "/")
	if rel == "" {
		rel = "index.html"
	}
	target := filepath.Join(root, filepath.FromSlash(rel))
	// Join already cleans, but the check is what stops ../../etc/passwd from
	// being served — and it is the one a hand-rolled static handler is usually
	// missing.
	if target != root && !strings.HasPrefix(target, root+string(os.PathSeparator)) {
		JSON(w, http.StatusForbidden, map[string]any{"error": "forbidden"})
		return true
	}
	if info, err := os.Stat(target); err == nil && info.IsDir() {
		target = filepath.Join(target, "index.html")
	}
	if _, err := os.Stat(target); err != nil {
		// A single-page app: unknown paths are routes, not missing files.
		index := filepath.Join(root, "index.html")
		if _, err := os.Stat(index); err != nil {
			return false
		}
		target = index
	}
	body, err := os.ReadFile(target)
	if err != nil {
		return false
	}
	ctype := mime.TypeByExtension(filepath.Ext(target))
	if ctype == "" {
		ctype = "application/octet-stream"
	}
	w.Header().Set("Content-Type", ctype)
	w.WriteHeader(http.StatusOK)
	if r.Method != http.MethodHead {
		_, _ = w.Write(body)
	}
	return true
}

// ---------------------------------------------------------------------------
// handler helpers
// ---------------------------------------------------------------------------

// MergeRoutes combines route maps, later entries winning. Handy for merging
// dispatch.Routes into an app's own.
func MergeRoutes(maps ...map[string]http.Handler) map[string]http.Handler {
	out := map[string]http.Handler{}
	for _, m := range maps {
		for k, v := range m {
			out[k] = v
		}
	}
	return out
}

// JSON writes v as a JSON response.
func JSON(w http.ResponseWriter, status int, v any) {
	raw, err := json.Marshal(v)
	if err != nil {
		raw = []byte(`{"error":"response is not encodable"}`)
		status = http.StatusInternalServerError
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write(raw)
}

// JSONHandler adapts a function that returns a value to an http.Handler: the
// value is written as JSON, and an error becomes 500 with {"error": …}.
//
// Return a [StatusError] to choose the status.
func JSONHandler(fn func(r *http.Request) (any, error)) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		out, err := fn(r)
		if err != nil {
			status := http.StatusInternalServerError
			var se *StatusError
			if errors.As(err, &se) {
				status = se.Status
			}
			JSON(w, status, map[string]any{"error": err.Error()})
			return
		}
		JSON(w, http.StatusOK, out)
	})
}

// StatusError is an error carrying the HTTP status a [JSONHandler] should use.
type StatusError struct {
	Status int
	Msg    string
}

func (e *StatusError) Error() string { return e.Msg }

// Statusf builds a [StatusError].
func Statusf(status int, format string, args ...any) *StatusError {
	return &StatusError{Status: status, Msg: fmt.Sprintf(format, args...)}
}

// Bind decodes a JSON request body into dst.
func Bind(r *http.Request, dst any) error {
	body, err := readBody(r)
	if err != nil {
		return err
	}
	if len(strings.TrimSpace(string(body))) == 0 {
		return nil
	}
	if err := json.Unmarshal(body, dst); err != nil {
		return Statusf(http.StatusBadRequest, "body is not valid JSON: %v", err)
	}
	return nil
}

func readBody(r *http.Request) ([]byte, error) {
	if r.Body == nil {
		return nil, nil
	}
	body, err := io.ReadAll(http.MaxBytesReader(nil, r.Body, MaxBodyBytes))
	if err != nil {
		return nil, Statusf(http.StatusBadRequest, "could not read request body: %v", err)
	}
	return body, nil
}
