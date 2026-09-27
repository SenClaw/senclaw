package senclaw

// The bridge wire contract.
//
// The daemon's SpaceAppBridgeBody requires a field named `action` and defines
// no alias for it. Sending anything else — `capability` was the mistake that
// prompted these tests in the other SDKs — is a 422 from serde before a line of
// handler code runs, which surfaces to an app author as "the bridge is down"
// rather than "you sent the wrong key". So the key is pinned here rather than
// trusted.

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

type seen struct {
	Method string
	Path   string
	Body   map[string]any
}

// fakeDaemon records what the SDK actually put on the wire.
type fakeDaemon struct {
	srv   *httptest.Server
	seen  []seen
	reply any
	// status, when non-zero, is the HTTP status to answer with.
	status int
}

func newFakeDaemon(t *testing.T, reply any) *fakeDaemon {
	t.Helper()
	d := &fakeDaemon{reply: reply}
	d.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		_ = json.NewDecoder(r.Body).Decode(&body)
		d.seen = append(d.seen, seen{Method: r.Method, Path: r.URL.Path, Body: body})
		w.Header().Set("Content-Type", "application/json")
		if d.status != 0 {
			w.WriteHeader(d.status)
		}
		_ = json.NewEncoder(w).Encode(d.reply)
	}))
	t.Cleanup(d.srv.Close)
	return d
}

func (d *fakeDaemon) client(t *testing.T) *Space {
	t.Helper()
	s, err := New(WithAppID("t"), WithBaseURL(d.srv.URL))
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	return s
}

func TestBridgeSendsActionNotCapability(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"status": "ok", "text": "hi"})
	if _, err := d.client(t).LLM(context.Background(), LLMRequest{Prompt: "q"}); err != nil {
		t.Fatalf("LLM: %v", err)
	}
	body := d.seen[0].Body
	if body["action"] != "llm.request" {
		t.Fatalf("action = %v, want llm.request", body["action"])
	}
	if _, wrong := body["capability"]; wrong {
		t.Fatal("sent `capability` — the daemon 422s on this")
	}
	payload := body["payload"].(map[string]any)
	if payload["prompt"] != "q" {
		t.Fatalf("prompt = %v", payload["prompt"])
	}
	// A caller who names no ceiling still gets one; the daemon's own default
	// is not something an app should have to know.
	if payload["maxTokens"] != float64(4000) {
		t.Fatalf("maxTokens = %v, want 4000", payload["maxTokens"])
	}
	if d.seen[0].Path != "/api/space/apps/t/bridge" {
		t.Fatalf("path = %s", d.seen[0].Path)
	}
}

func TestLLMDetailedReturnsUsageAndFinish(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{
		"status": "ok", "text": "hello", "model": "m1", "finish": "length",
		"usage": map[string]any{"inputTokens": 12, "outputTokens": 3, "cacheReadTokens": 9},
	})
	r, err := d.client(t).LLMDetailed(context.Background(), LLMRequest{Prompt: "q"})
	if err != nil {
		t.Fatalf("LLMDetailed: %v", err)
	}
	if r.Text != "hello" || r.Model != "m1" || r.Finish != "length" {
		t.Fatalf("reply = %+v", r)
	}
	if r.Usage == nil || r.Usage.InputTokens != 12 || r.Usage.CacheReadTokens != 9 {
		t.Fatalf("usage = %+v", r.Usage)
	}
	// Unreported by this provider, and 0 is the right reading of absent.
	if r.Usage.CacheCreationTokens != 0 {
		t.Fatalf("cacheCreation = %d", r.Usage.CacheCreationTokens)
	}
}

func TestLLMDetailedUsageIsNilWhenProviderReportsNone(t *testing.T) {
	// Distinct from "zero tokens" — some local models report nothing at all,
	// and recording that as 0 would quietly understate the daemon's totals.
	d := newFakeDaemon(t, map[string]any{"status": "ok", "text": "x", "model": "local"})
	r, err := d.client(t).LLMDetailed(context.Background(), LLMRequest{Prompt: "q"})
	if err != nil {
		t.Fatalf("LLMDetailed: %v", err)
	}
	if r.Usage != nil {
		t.Fatalf("usage = %+v, want nil", r.Usage)
	}
}

func TestLLMErrorsOnTruncatedReply(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"status": "ok", "text": "partial", "finish": "length"})
	_, err := d.client(t).LLM(context.Background(), LLMRequest{Prompt: "q"})
	if err == nil {
		t.Fatal("a truncated reply must not read as a short answer")
	}
	if !strings.Contains(err.Error(), "truncated") {
		t.Fatalf("error = %v", err)
	}
}

func TestBridgeErrorEnvelopeFailsDespiteHTTP200(t *testing.T) {
	// The daemon answers a failed action with HTTP 200 and status:"error".
	// Reading only the HTTP code turns a dead provider into an empty string,
	// which downstream reads as "the model had nothing to say".
	d := newFakeDaemon(t, map[string]any{"status": "error", "message": "LLM HTTP 404 Not Found"})
	_, err := d.client(t).LLM(context.Background(), LLMRequest{Prompt: "q"})
	if err == nil || !strings.Contains(err.Error(), "404") {
		t.Fatalf("error = %v, want the daemon's message", err)
	}
}

func TestBridgePendingNamesTheRealProblem(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"status": "pending"})
	_, err := d.client(t).KnowledgeRecall(context.Background(), RecallQuery{Query: "q"})
	if err == nil || !strings.Contains(err.Error(), "not enabled") {
		t.Fatalf("error = %v, want `not enabled`", err)
	}
}

func TestBridgePassesThroughAPayloadWithNoStatusField(t *testing.T) {
	// Not every action answers with an envelope; those must not be mistaken
	// for failures just because `status` is absent.
	d := newFakeDaemon(t, map[string]any{"hits": []any{}})
	hits, err := d.client(t).KnowledgeSearch(context.Background(), "q", "", 0)
	if err != nil {
		t.Fatalf("KnowledgeSearch: %v", err)
	}
	if len(hits) != 0 {
		t.Fatalf("hits = %v", hits)
	}
}

func TestKnowledgeCallsUseTheRightActions(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{
		"status": "ok",
		"hits":   []any{map[string]any{"name": "n", "summary": "s", "score": 0.5}},
	})
	c := d.client(t)
	ctx := context.Background()
	if err := c.KnowledgeSave(ctx, Memory{Text: "remember this", Space: "proj", Tags: []string{"a"}}); err != nil {
		t.Fatalf("KnowledgeSave: %v", err)
	}
	hits, err := c.KnowledgeSearch(ctx, "q", "proj", 3)
	if err != nil {
		t.Fatalf("KnowledgeSearch: %v", err)
	}
	if got := d.seen[0].Body["action"]; got != "knowledge.save" {
		t.Fatalf("action[0] = %v", got)
	}
	if got := d.seen[1].Body["action"]; got != "knowledge.search" {
		t.Fatalf("action[1] = %v", got)
	}
	save := d.seen[0].Body["payload"].(map[string]any)
	if tags, _ := save["tags"].([]any); len(tags) != 1 || tags[0] != "a" {
		t.Fatalf("tags = %v", save["tags"])
	}
	search := d.seen[1].Body["payload"].(map[string]any)
	if search["limit"] != float64(3) {
		t.Fatalf("limit = %v", search["limit"])
	}
	if len(hits) != 1 || hits[0].Name != "n" || hits[0].Score != 0.5 {
		t.Fatalf("hits = %+v", hits)
	}
}

func TestKnowledgeOmitsSpaceWhenNotGiven(t *testing.T) {
	// Omitted means "this app's own private space". Sending an empty string
	// would be a different thing to the daemon than not sending the key.
	d := newFakeDaemon(t, map[string]any{"status": "ok"})
	if err := d.client(t).KnowledgeSave(context.Background(), Memory{Text: "x"}); err != nil {
		t.Fatalf("KnowledgeSave: %v", err)
	}
	payload := d.seen[0].Body["payload"].(map[string]any)
	if _, present := payload["space"]; present {
		t.Fatal("space was sent when none was named")
	}
}

func TestUsageReportNeverFailsTheCaller(t *testing.T) {
	// Fire-and-forget: accounting must not take down the work it describes.
	// Nothing is listening on this port, and the call still returns.
	s, err := New(WithAppID("t"), WithBaseURL("http://127.0.0.1:9"))
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	s.UsageReport(context.Background(), Usage{Model: "m", Provider: "p", InputTokens: 1, OutputTokens: 2})
}

func TestListModelsReadsLLMConfig(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{
		"activeId": "a1",
		"configs": []any{
			map[string]any{"id": "a1", "modelName": "Sonnet", "adapt": "anthropic"},
			map[string]any{"nope": 1},
		},
	})
	active, models, err := d.client(t).ListModels(context.Background())
	if err != nil {
		t.Fatalf("ListModels: %v", err)
	}
	if active != "a1" {
		t.Fatalf("active = %q", active)
	}
	if len(models) != 1 {
		t.Fatalf("an entry with no id is not a model: %+v", models)
	}
	if models[0].ID != "a1" || models[0].Provider != "anthropic" {
		t.Fatalf("model = %+v", models[0])
	}
	if d.seen[0].Path != "/api/llm-config" {
		t.Fatalf("path = %s", d.seen[0].Path)
	}
}

func TestGetConfigTreatsAMissingKeyAsNotFoundRatherThanAnError(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"error": "Config key not found"})
	d.status = http.StatusNotFound
	var out string
	found, err := d.client(t).GetConfig(context.Background(), "nope", &out)
	if err != nil {
		t.Fatalf("a never-set key is not an error: %v", err)
	}
	if found {
		t.Fatal("found = true for a key that was never set")
	}
}

func TestGetConfigUnmarshalsTheStoredValue(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"key": "prefs", "value": map[string]any{"theme": "dark"}})
	var prefs struct {
		Theme string `json:"theme"`
	}
	found, err := d.client(t).GetConfig(context.Background(), "prefs", &prefs)
	if err != nil || !found {
		t.Fatalf("found=%v err=%v", found, err)
	}
	if prefs.Theme != "dark" {
		t.Fatalf("theme = %q", prefs.Theme)
	}
}

func TestSQLiteSendsParametersRatherThanFormattingThem(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"rows": []any{map[string]any{"id": 1, "title": "t"}}})
	var rows []struct {
		ID    int64  `json:"id"`
		Title string `json:"title"`
	}
	err := d.client(t).SQLiteScan(context.Background(), &rows,
		"SELECT id, title FROM todos WHERE done = ?", 0)
	if err != nil {
		t.Fatalf("SQLiteScan: %v", err)
	}
	if len(rows) != 1 || rows[0].Title != "t" {
		t.Fatalf("rows = %+v", rows)
	}
	body := d.seen[0].Body
	if body["sql"] == nil || len(body["params"].([]any)) != 1 {
		t.Fatalf("body = %v", body)
	}
}

func TestSQLiteReportsWritesRatherThanRows(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"rowsAffected": 2, "lastInsertRowId": 17})
	res, err := d.client(t).SQLite(context.Background(), "UPDATE todos SET done = 1")
	if err != nil {
		t.Fatalf("SQLite: %v", err)
	}
	if res.RowsAffected != 2 || res.LastInsertRowID != 17 {
		t.Fatalf("res = %+v", res)
	}
}

func TestAnHTTPErrorCarriesItsStatusAndTheDaemonsMessage(t *testing.T) {
	d := newFakeDaemon(t, map[string]any{"error": "Invalid app id"})
	d.status = http.StatusBadRequest
	_, err := d.client(t).ListConfig(context.Background())
	if StatusOf(err) != http.StatusBadRequest {
		t.Fatalf("status = %d, err = %v", StatusOf(err), err)
	}
	if !strings.Contains(err.Error(), "Invalid app id") {
		t.Fatalf("error = %v, want the daemon's own message", err)
	}
}

func TestEnvHelpers(t *testing.T) {
	t.Setenv("SENCLAW_BIND_HOST", "")
	if got := BindHost(); got != "127.0.0.1" {
		t.Fatalf("BindHost = %q — an app has no auth of its own; loopback is the default", got)
	}
	t.Setenv("SENCLAW_BIND_HOST", "0.0.0.0")
	if got := BindHost(); got != "0.0.0.0" {
		t.Fatalf("BindHost = %q, want the operator's explicit opt-out", got)
	}

	t.Setenv("PORT", "4820")
	if p, err := Port(0); err != nil || p != 4820 {
		t.Fatalf("Port = %d, %v", p, err)
	}
	t.Setenv("PORT", "")
	if _, err := Port(0); err == nil {
		t.Fatal("PORT unset with no fallback must fail loudly")
	}
	if p, _ := Port(4810); p != 4810 {
		t.Fatalf("fallback ignored: %d", p)
	}

	t.Setenv("SENCLAW_SPACE_APP_ID", "")
	if _, err := New(); err == nil {
		t.Fatal("an app that cannot resolve its own id must not start")
	}
	t.Setenv("SENCLAW_SPACE_APP_ID", "from-env")
	s, err := New()
	if err != nil || s.AppID != "from-env" {
		t.Fatalf("app id = %q, %v", s.AppID, err)
	}
	if s.BaseURL != DefaultBaseURL {
		t.Fatalf("base url = %q", s.BaseURL)
	}
}
