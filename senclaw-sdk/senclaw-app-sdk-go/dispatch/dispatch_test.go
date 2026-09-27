package dispatch

// The wire shape is the whole contract here: the engine parses these payloads
// with serde, so a camelCase key or a null where a Vec was expected is dropped
// or rejected without anything in this package noticing.

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

type todos struct {
	Unleased
	finalized []struct {
		id      string
		outcome Outcome
	}
	beats []string
	fail  bool
}

func (t *todos) ClaimReady(_ context.Context, c Capacity) ([]WorkItem, error) {
	if t.fail {
		return nil, errString("db is gone")
	}
	items := make([]WorkItem, 0, c.Total)
	for i := 0; i < c.Total; i++ {
		item := WorkItem{
			ID:          "t" + string(rune('0'+i)),
			Prompt:      "do it",
			Assignee:    "worker",
			MCP:         []MCPServerSpec{Stdio("kanban", "senclaw", []string{"kanban-server"}, nil)},
			Workspace:   Worktree("/repo", "main"),
			TimeoutSecs: 60,
		}
		if i > 0 {
			item.DependsOn = []string{"t0"}
		}
		items = append(items, item)
	}
	return items, nil
}

func (t *todos) Heartbeat(_ context.Context, id string) error {
	t.beats = append(t.beats, id)
	return nil
}

func (t *todos) Reclaim(context.Context) ([]string, error) { return []string{"stale-1"}, nil }

func (t *todos) Finalize(_ context.Context, id string, o Outcome) error {
	t.finalized = append(t.finalized, struct {
		id      string
		outcome Outcome
	}{id, o})
	return nil
}

type errString string

func (e errString) Error() string { return string(e) }

func post(t *testing.T, routes map[string]http.Handler, path, body string) (int, string) {
	t.Helper()
	h, ok := routes["POST "+path]
	if !ok {
		t.Fatalf("no route for POST %s (have %v)", path, keys(routes))
	}
	req := httptest.NewRequest(http.MethodPost, path, strings.NewReader(body))
	w := httptest.NewRecorder()
	h.ServeHTTP(w, req)
	return w.Code, w.Body.String()
}

func keys(m map[string]http.Handler) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}

func TestPollSerialisesTheRustWireShape(t *testing.T) {
	_, body := post(t, Routes(&todos{}, ""), "/api/dispatch/poll",
		`{"capacity":{"total":2,"per_assignee":1}}`)

	var items []map[string]any
	if err := json.Unmarshal([]byte(body), &items); err != nil {
		t.Fatalf("poll body is not a JSON array: %v (%s)", err, body)
	}
	if len(items) != 2 {
		t.Fatalf("items = %d", len(items))
	}
	it := items[1]
	// snake_case, exactly as serde expects — camelCase is dropped silently,
	// which would surface as a dependency that never held.
	deps := it["depends_on"].([]any)
	if len(deps) != 1 || deps[0] != "t0" {
		t.Fatalf("depends_on = %v", it["depends_on"])
	}
	if it["timeout_secs"] != float64(60) {
		t.Fatalf("timeout_secs = %v", it["timeout_secs"])
	}
	ws := it["workspace"].(map[string]any)
	if ws["kind"] != "worktree" || ws["repo"] != "/repo" || ws["branch"] != "main" {
		t.Fatalf("workspace = %v", ws)
	}
	mcp := it["mcp"].([]any)[0].(map[string]any)
	if mcp["transport"] != "stdio" || mcp["args"].([]any)[0] != "kanban-server" {
		t.Fatalf("mcp = %v", mcp)
	}
}

func TestAnItemWithNothingSetStillSerialisesWhatSerdeNeeds(t *testing.T) {
	// Vec and the Workspace enum both refuse an explicit null, so a bare item
	// must go out with [] and a scratch workspace rather than nulls.
	raw, err := json.Marshal(WorkItem{ID: "t1", Prompt: "go"})
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]any
	if err := json.Unmarshal(raw, &m); err != nil {
		t.Fatal(err)
	}
	if _, ok := m["mcp"].([]any); !ok {
		t.Fatalf("mcp = %v, want []", m["mcp"])
	}
	if _, ok := m["depends_on"].([]any); !ok {
		t.Fatalf("depends_on = %v, want []", m["depends_on"])
	}
	if m["workspace"].(map[string]any)["kind"] != "scratch" {
		t.Fatalf("workspace = %v", m["workspace"])
	}
}

func TestFinalizeHeartbeatAndReclaim(t *testing.T) {
	p := &todos{}
	routes := Routes(p, "")

	post(t, routes, "/api/dispatch/heartbeat", `{"item_id":"t1"}`)
	if len(p.beats) != 1 || p.beats[0] != "t1" {
		t.Fatalf("beats = %v", p.beats)
	}

	_, body := post(t, routes, "/api/dispatch/reclaim", `{}`)
	if !strings.Contains(body, "stale-1") {
		t.Fatalf("reclaim = %s", body)
	}

	post(t, routes, "/api/dispatch/finalize",
		`{"item_id":"t1","outcome":{"status":"completed","summary":"done","metadata":{"n":1}}}`)
	if len(p.finalized) != 1 || p.finalized[0].id != "t1" {
		t.Fatalf("finalized = %v", p.finalized)
	}
	if p.finalized[0].outcome.Status() != "completed" {
		t.Fatalf("outcome = %v", p.finalized[0].outcome)
	}
}

func TestAFinalizeWithNoOutcomeIsNotSilentlyASuccess(t *testing.T) {
	p := &todos{}
	post(t, Routes(p, ""), "/api/dispatch/finalize", `{"item_id":"t1"}`)
	if p.finalized[0].outcome.Status() != "failed" {
		t.Fatalf("outcome = %v — a missing outcome must not read as done", p.finalized[0].outcome)
	}
}

func TestProviderErrorBecomes500NotAReset(t *testing.T) {
	// The engine reads `error` from the body and backs off; a dropped
	// connection just looks like the app is down.
	code, body := post(t, Routes(&todos{fail: true}, ""), "/api/dispatch/poll", `{}`)
	if code != http.StatusInternalServerError || !strings.Contains(body, "db is gone") {
		t.Fatalf("%d %s", code, body)
	}
}

func TestAnEmptyClaimIsAnEmptyArrayNotNull(t *testing.T) {
	code, body := post(t, Routes(&todos{}, ""), "/api/dispatch/poll", `{}`)
	if code != 200 || strings.TrimSpace(body) != "[]" {
		t.Fatalf("%d %q", code, body)
	}
}

func TestPrefixIsConfigurable(t *testing.T) {
	routes := Routes(&todos{}, "custom/queue")
	if _, ok := routes["POST /custom/queue/poll"]; !ok {
		t.Fatalf("routes = %v", keys(routes))
	}
}

func TestOutcomeConstructors(t *testing.T) {
	if Blocked("waiting on a human").Status() != "blocked" {
		t.Fatal("blocked")
	}
	if Failed("boom").Status() != "failed" {
		t.Fatal("failed")
	}
	if TimedOut().Status() != "timed_out" {
		t.Fatal("timed_out — snake_case, as serde renames it")
	}
}
