package senclaw

// The app's identity on the wire, both directions.
//
// Outbound: every daemon call carries the access token, or the daemon cannot
// tell this app's calls from any other local process's. Inbound: with the guard
// on, the app's own port answers only the daemon.

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestClientSendsTokenAndVersionOnEveryCall(t *testing.T) {
	var got http.Header
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Clone()
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"value":1}`))
	}))
	defer srv.Close()

	s, err := New(WithAppID("t"), WithBaseURL(srv.URL), WithAppToken("sca_"+strings.Repeat("a", 64)))
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	var out int
	if _, err := s.GetConfig(context.Background(), "k", &out); err != nil {
		t.Fatalf("GetConfig: %v", err)
	}
	if got.Get(HeaderAppToken) != "sca_"+strings.Repeat("a", 64) {
		t.Fatalf("%s = %q", HeaderAppToken, got.Get(HeaderAppToken))
	}
	if got.Get(HeaderAPIVersion) == "" {
		t.Fatalf("%s missing — the daemon cannot negotiate a contract it is not told about", HeaderAPIVersion)
	}
}

func TestClientOmitsTheHeaderWhenThereIsNoToken(t *testing.T) {
	// Running the app by hand: no token in the environment. Sending an empty
	// header would be worse than sending none — the daemon would try to resolve
	// "" and 401 a call that `off` mode would have served.
	var got http.Header
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Clone()
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{}`))
	}))
	defer srv.Close()

	s, err := New(WithAppID("t"), WithBaseURL(srv.URL), WithAppToken(""))
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	if _, err := s.ListConfig(context.Background()); err != nil {
		t.Fatalf("ListConfig: %v", err)
	}
	if _, present := got[http.CanonicalHeaderKey(HeaderAppToken)]; present {
		t.Fatal("sent an empty app-token header")
	}
}

func TestGuardRefusesEveryoneButTheDaemon(t *testing.T) {
	const token = "sca_" + "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
	h := RequireAppToken(token, []string{"/health", "/public/*"})(
		http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
			w.WriteHeader(http.StatusOK)
			_, _ = w.Write([]byte("ok"))
		}))
	srv := httptest.NewServer(h)
	defer srv.Close()

	call := func(path, tok string) int {
		req, _ := http.NewRequest(http.MethodGet, srv.URL+path, nil)
		if tok != "" {
			req.Header.Set(HeaderAppToken, tok)
		}
		res, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatalf("call %s: %v", path, err)
		}
		defer res.Body.Close()
		return res.StatusCode
	}

	if code := call("/api/notes", token); code != 200 {
		t.Fatalf("the daemon's own request = %d, want 200", code)
	}
	// What this feature exists to stop: another local process hitting the port.
	if code := call("/api/notes", ""); code != 401 {
		t.Fatalf("tokenless = %d, want 401", code)
	}
	if code := call("/api/notes", "sca_"+strings.Repeat("f", 64)); code != 401 {
		t.Fatalf("wrong token = %d, want 401", code)
	}
	// The health check runs before anything is proxied; locking it out would
	// make the app look permanently dead to the daemon.
	if code := call("/health", ""); code != 200 {
		t.Fatalf("health = %d, want 200", code)
	}
	if code := call("/public/logo.png", ""); code != 200 {
		t.Fatalf("skipped prefix = %d, want 200", code)
	}
}

func TestGuardIsInertWithoutAToken(t *testing.T) {
	// A bare `go run .` has no token. Refusing everything would mean the guard
	// turns "not launched by SenClaw" into "app is down".
	srv := httptest.NewServer(RequireAppToken("", nil)(
		http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.WriteHeader(200) })))
	defer srv.Close()
	res, err := http.Get(srv.URL + "/api/anything")
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		t.Fatalf("status = %d, want 200", res.StatusCode)
	}
}

func TestServeConfigWiresTheGuard(t *testing.T) {
	t.Setenv(EnvAppToken, "sca_"+strings.Repeat("b", 64))
	srv := httptest.NewServer(Handler(Config{
		RequireAppToken: true,
		HealthPath:      "/api/status",
		Routes: map[string]http.Handler{
			"GET /api/notes": JSONHandler(func(*http.Request) (any, error) { return "notes", nil }),
		},
	}))
	defer srv.Close()

	res, err := http.Get(srv.URL + "/api/notes")
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	defer res.Body.Close()
	if res.StatusCode != 401 {
		t.Fatalf("status = %d, want 401", res.StatusCode)
	}
	// HealthPath is exempt without the app having to list it.
	health, err := http.Get(srv.URL + "/api/status")
	if err != nil {
		t.Fatalf("health: %v", err)
	}
	defer health.Body.Close()
	if health.StatusCode != 200 {
		t.Fatalf("health status = %d, want 200", health.StatusCode)
	}
}

func TestAPIVersionFromEnvFallsBackToTheCompiledOne(t *testing.T) {
	t.Setenv(EnvAPIVersion, "")
	if v := APIVersionFromEnv(); v != APIVersion {
		t.Fatalf("v = %d, want %d", v, APIVersion)
	}
	t.Setenv(EnvAPIVersion, "7")
	if v := APIVersionFromEnv(); v != 7 {
		t.Fatalf("v = %d, want 7", v)
	}
	// Garbage must not become 0, which would drop the header entirely.
	t.Setenv(EnvAPIVersion, "v2")
	if v := APIVersionFromEnv(); v != APIVersion {
		t.Fatalf("v = %d, want %d", v, APIVersion)
	}
}
