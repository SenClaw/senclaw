package llm

// The bytes on the wire are the contract: SenClaw's OpenAI adapter accumulates
// delta.tool_calls[].function.{name,arguments} by concatenation keyed on index,
// reads usage from a chunk with empty choices, and needs the stream terminated
// with [DONE]. So the router tests drive a real request through Handler and read
// the bytes that come back, exactly as the daemon would.

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

// ---------------------------------------------------------------------------
// a fake provider
// ---------------------------------------------------------------------------

func card() ModelCard { return NewModelCard("m", 4096, 512, false) }

type fake struct {
	chunks []Chunk
	err    error
}

func (f *fake) Models() []ModelCard { return []ModelCard{card()} }

func (f *fake) Chat(_ context.Context, _ ChatRequest, sink *Sink) error {
	for _, c := range f.chunks {
		sink.Send(c)
	}
	return f.err
}

// postChat drives one chat/completions request through the real Handler and
// returns the raw response body.
func postChat(t *testing.T, provider Provider, chunks []Chunk, stream bool) (int, string) {
	t.Helper()
	body := `{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":` +
		strconv.FormatBool(stream) + `}`
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	w := httptest.NewRecorder()
	if provider == nil {
		provider = &fake{chunks: chunks}
	}
	Handler(provider).ServeHTTP(w, req)
	return w.Code, w.Body.String()
}

// ssePayloads parses an SSE body back into the JSON objects a consumer sees,
// dropping the [DONE] terminator.
func ssePayloads(t *testing.T, raw string) []map[string]any {
	t.Helper()
	var out []map[string]any
	for _, line := range strings.Split(raw, "\n") {
		data, ok := strings.CutPrefix(line, "data: ")
		if !ok || data == "[DONE]" {
			continue
		}
		var m map[string]any
		if err := json.Unmarshal([]byte(data), &m); err != nil {
			t.Fatalf("every data: line must be JSON, got %q: %v", data, err)
		}
		out = append(out, m)
	}
	return out
}

func choice0(p map[string]any) map[string]any {
	choices, _ := p["choices"].([]any)
	if len(choices) == 0 {
		return nil
	}
	c, _ := choices[0].(map[string]any)
	return c
}

// ---------------------------------------------------------------------------
// request parsing
// ---------------------------------------------------------------------------

func TestParseChatRequestRejectsMissingMessages(t *testing.T) {
	cases := []struct {
		name string
		body string
	}{
		{"no messages field", `{"model":"m"}`},
		{"empty messages", `{"model":"m","messages":[]}`},
		{"no model", `{"messages":[{"role":"user"}]}`},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := ParseChatRequest([]byte(tc.body)); err == nil {
				t.Fatalf("%s must be refused rather than run", tc.name)
			}
		})
	}
}

func TestParseChatRequestReadsBothCeilingSpellings(t *testing.T) {
	cases := []struct {
		name string
		body string
		want uint32
	}{
		{"max_tokens", `{"model":"m","messages":[{"role":"user"}],"max_tokens":100}`, 100},
		// The newer spelling wins when a client sends both.
		{"max_completion_tokens wins", `{"model":"m","messages":[{"role":"user"}],"max_tokens":100,"max_completion_tokens":200}`, 200},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			req, err := ParseChatRequest([]byte(tc.body))
			if err != nil {
				t.Fatal(err)
			}
			if req.MaxTokens == nil || *req.MaxTokens != tc.want {
				t.Fatalf("MaxTokens = %v, want %d", req.MaxTokens, tc.want)
			}
		})
	}
}

// Image turns arrive as an array of content parts. Keeping Messages as raw JSON
// is what lets the image survive a parse that a typed message would flatten.
func TestMultipartContentSurvivesTheParse(t *testing.T) {
	body := `{"model":"m","messages":[{"role":"user","content":[` +
		`{"type":"text","text":"what is this"},` +
		`{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}]}]}`
	req, err := ParseChatRequest([]byte(body))
	if err != nil {
		t.Fatal(err)
	}
	var msg struct {
		Content []map[string]any `json:"content"`
	}
	if err := json.Unmarshal(req.Messages[0], &msg); err != nil {
		t.Fatalf("message did not survive as raw JSON: %v", err)
	}
	if len(msg.Content) != 2 {
		t.Fatalf("content parts = %d, want 2", len(msg.Content))
	}
	img, _ := msg.Content[1]["image_url"].(map[string]any)
	if img["url"] != "data:image/png;base64,AAA" {
		t.Fatalf("image url = %v", img["url"])
	}
}

// ---------------------------------------------------------------------------
// event rendering (internal)
// ---------------------------------------------------------------------------

func TestEmptyTextEmitsNoChunk(t *testing.T) {
	index := 0
	if _, emit := chunkToEvent("id", "m", Text(""), &index); emit {
		t.Fatal("empty text must emit nothing")
	}
	if _, emit := chunkToEvent("id", "m", Reasoning(""), &index); emit {
		t.Fatal("empty reasoning must emit nothing")
	}
	if index != 0 {
		t.Fatalf("a dropped chunk must not consume a tool-call index, got %d", index)
	}
}

func TestNonStreamBodyFinishReason(t *testing.T) {
	calls := []any{map[string]any{"id": "c1"}}
	if fr := choice0(nonStreamBody("m", "", "", calls, nil))["finish_reason"]; fr != "tool_calls" {
		t.Fatalf("a tool-call turn must finish tool_calls, got %v", fr)
	}
	if fr := choice0(nonStreamBody("m", "hi", "", nil, nil))["finish_reason"]; fr != "stop" {
		t.Fatalf("a text turn must finish stop, got %v", fr)
	}
}

func TestNonStreamBodyOmitsEmptyReasoning(t *testing.T) {
	msg, _ := choice0(nonStreamBody("m", "hi", "", nil, nil))["message"].(map[string]any)
	if _, ok := msg["reasoning_content"]; ok {
		t.Fatal("empty reasoning must be omitted, not sent blank")
	}
	msg, _ = choice0(nonStreamBody("m", "hi", "why", nil, nil))["message"].(map[string]any)
	if msg["reasoning_content"] != "why" {
		t.Fatalf("reasoning_content = %v", msg["reasoning_content"])
	}
}

// ---------------------------------------------------------------------------
// the streaming router
// ---------------------------------------------------------------------------

// Two calls streamed at the same index would concatenate into
// `get_weatherget_time` with both argument objects glued together — no test of
// the event builder alone would notice, so this reads the wire.
func TestTwoToolCallsStreamAtDistinctIndices(t *testing.T) {
	_, raw := postChat(t, nil, []Chunk{
		ToolCall("call_a", "get_weather", `{"city":"Hanoi"}`),
		ToolCall("call_b", "get_time", "{}"),
	}, true)

	var calls []map[string]any
	for _, p := range ssePayloads(t, raw) {
		delta, _ := choice0(p)["delta"].(map[string]any)
		tcs, _ := delta["tool_calls"].([]any)
		if len(tcs) == 0 {
			continue
		}
		tc, _ := tcs[0].(map[string]any)
		calls = append(calls, tc)
	}

	if len(calls) != 2 {
		t.Fatalf("calls = %d, want 2", len(calls))
	}
	if calls[0]["index"] != float64(0) || calls[0]["function"].(map[string]any)["name"] != "get_weather" {
		t.Fatalf("first call = %v", calls[0])
	}
	if calls[1]["index"] != float64(1) {
		t.Fatalf("a reused index welds the two calls together, got index %v", calls[1]["index"])
	}
	if calls[1]["function"].(map[string]any)["name"] != "get_time" {
		t.Fatalf("second call = %v", calls[1])
	}
}

func TestStreamAlwaysTerminatesWithDone(t *testing.T) {
	_, raw := postChat(t, nil, []Chunk{Text("hi")}, true)
	if !strings.HasSuffix(strings.TrimRight(raw, "\n"), "data: [DONE]") {
		t.Fatalf("an unterminated stream reads as a hang, not a failure:\n%s", raw)
	}
}

// Usage rides a chunk with an empty choices array — the shape
// stream_options.include_usage produces, and the only place the consumer looks.
func TestUsageArrivesOnItsOwnChunkWithNoChoices(t *testing.T) {
	_, raw := postChat(t, nil, []Chunk{Text("hi"), Usage(12, 3)}, true)

	var usage map[string]any
	for _, p := range ssePayloads(t, raw) {
		if _, ok := p["usage"]; ok {
			usage = p
			break
		}
	}
	if usage == nil {
		t.Fatal("a usage chunk must be emitted")
	}
	u := usage["usage"].(map[string]any)
	if u["prompt_tokens"] != float64(12) || u["total_tokens"] != float64(15) {
		t.Fatalf("usage = %v", u)
	}
	if choices, _ := usage["choices"].([]any); len(choices) != 0 {
		t.Fatalf("usage chunk must carry empty choices, got %v", usage["choices"])
	}
}

// A generation that fails after emitting text sends an error chunk, not a silent
// truncation — the status line already went out and cannot become a 5xx — and
// still terminates with [DONE].
func TestStreamFailureBecomesAnErrorChunkNotSilentTruncation(t *testing.T) {
	code, raw := postChat(t, &fake{chunks: []Chunk{Text("partial")}, err: errString("boom")}, nil, true)
	if code != http.StatusOK {
		t.Fatalf("a stream that already sent bytes cannot 5xx, got %d", code)
	}
	var sawError bool
	for _, p := range ssePayloads(t, raw) {
		if choice0(p)["finish_reason"] == "error" {
			sawError = true
		}
	}
	if !sawError {
		t.Fatalf("a failed generation must emit an error chunk:\n%s", raw)
	}
	if !strings.HasSuffix(strings.TrimRight(raw, "\n"), "data: [DONE]") {
		t.Fatalf("even a failed stream must terminate with [DONE]:\n%s", raw)
	}
}

// ---------------------------------------------------------------------------
// the non-stream router
// ---------------------------------------------------------------------------

func TestNonStreamReturnsOneAssembledMessage(t *testing.T) {
	_, raw := postChat(t, nil, []Chunk{
		Reasoning("thinking"),
		Text("he"),
		Text("llo"),
	}, false)

	var body map[string]any
	if err := json.Unmarshal([]byte(raw), &body); err != nil {
		t.Fatalf("non-stream body is not one JSON object: %v", err)
	}
	if body["object"] != "chat.completion" {
		t.Fatalf("object = %v", body["object"])
	}
	msg := choice0(body)["message"].(map[string]any)
	if msg["content"] != "hello" {
		t.Fatalf("text deltas must be concatenated, got %v", msg["content"])
	}
	if msg["reasoning_content"] != "thinking" {
		t.Fatalf("reasoning_content = %v", msg["reasoning_content"])
	}
}

func TestNonStreamChatErrorBecomes500(t *testing.T) {
	// A dead generation must not answer 200 with half a message.
	code, body := postChat(t, &fake{chunks: []Chunk{Text("half")}, err: errString("db is gone")}, nil, false)
	if code != http.StatusInternalServerError || !strings.Contains(body, "db is gone") {
		t.Fatalf("want 500 with the error, got %d %s", code, body)
	}
}

func TestUnknownModelIs404NotASilentDefault(t *testing.T) {
	body := `{"model":"nope","messages":[{"role":"user"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	w := httptest.NewRecorder()
	Handler(&fake{}).ServeHTTP(w, req)
	if w.Code != http.StatusNotFound {
		t.Fatalf("unknown model = %d, want 404", w.Code)
	}
}

// The daemon builds a picker entry from /v1/models, so the capability fields
// must survive the hop — vision decides between real image blocks and OCR.
func TestModelsEndpointCarriesCapabilityFields(t *testing.T) {
	req := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	w := httptest.NewRecorder()
	Handler(&fake{}).ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d", w.Code)
	}
	var body struct {
		Data []map[string]any `json:"data"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if len(body.Data) != 1 {
		t.Fatalf("data = %d models", len(body.Data))
	}
	m := body.Data[0]
	if m["id"] != "m" || m["context_length"] != float64(4096) || m["vision"] != false {
		t.Fatalf("model entry missing capability fields: %v", m)
	}
}

// Routes exposes the same two handlers under the keys senclaw.Config.Routes
// wants, so they merge into an app's own route table.
func TestRoutesExposeBothEndpoints(t *testing.T) {
	routes := Routes(&fake{})
	for _, key := range []string{"GET /v1/models", "POST /v1/chat/completions"} {
		if _, ok := routes[key]; !ok {
			t.Fatalf("missing route %q", key)
		}
	}
}

// ---------------------------------------------------------------------------
// model card + cache
// ---------------------------------------------------------------------------

func TestToolsDefaultsTrueWhenAbsentOnDecode(t *testing.T) {
	var m ModelCard
	if err := json.Unmarshal([]byte(`{"id":"x","context_length":8,"max_output_tokens":4,"vision":false}`), &m); err != nil {
		t.Fatal(err)
	}
	if !m.Tools {
		t.Fatal("an absent tools field must decode as true, not false")
	}
	if err := json.Unmarshal([]byte(`{"id":"x","tools":false}`), &m); err != nil {
		t.Fatal(err)
	}
	if m.Tools {
		t.Fatal("an explicit tools:false must stay false")
	}
}

func TestPublishModelsRefusesEmptyAndNeverClobbers(t *testing.T) {
	dir := t.TempDir()
	if err := PublishModels(dir, []ModelCard{card()}); err != nil {
		t.Fatal(err)
	}
	good, err := os.ReadFile(filepath.Join(dir, ModelsCachePath))
	if err != nil {
		t.Fatalf("cache was not written: %v", err)
	}

	if err := PublishModels(dir, nil); err == nil {
		t.Fatal("an empty model list must be refused")
	}
	after, err := os.ReadFile(filepath.Join(dir, ModelsCachePath))
	if err != nil {
		t.Fatal(err)
	}
	if string(after) != string(good) {
		t.Fatal("a failed publish must leave the good cache intact")
	}
}

func TestPublishedCardsRoundTrip(t *testing.T) {
	dir := t.TempDir()
	m := NewModelCard("gemma", 128000, 8192, true).WithDisplayName("Gemma 4")
	if err := PublishModels(dir, []ModelCard{m}); err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(filepath.Join(dir, ModelsCachePath))
	if err != nil {
		t.Fatal(err)
	}
	var back struct {
		Models []ModelCard `json:"models"`
	}
	if err := json.Unmarshal(raw, &back); err != nil {
		t.Fatal(err)
	}
	if len(back.Models) != 1 {
		t.Fatalf("models = %d", len(back.Models))
	}
	got := back.Models[0]
	if got.ID != "gemma" || got.DisplayName != "Gemma 4" || !got.Vision {
		t.Fatalf("card did not round-trip: %+v", got)
	}
	if !got.Tools {
		t.Fatal("tools must round-trip as true")
	}
}

type errString string

func (e errString) Error() string { return string(e) }
