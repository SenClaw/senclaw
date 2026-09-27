// Package llm lets a Go Space App serve an LLM, so SenClaw routes agent turns
// to it.
//
// This is the reverse of the AI bridge. There, an app asks the daemon for a
// completion. Here the app *is* the model: it declares an `llm` block in its
// senclaw-manifest.json, the daemon registers the models it advertises into the
// same picker as every remote provider, and agent turns arrive over HTTP.
//
//	type Mlx struct{ /* … */ }
//
//	func (m *Mlx) Models() []llm.ModelCard {
//		return []llm.ModelCard{llm.NewModelCard("gemma-4-e2b-it-4bit", 128000, 8192, true)}
//	}
//	func (m *Mlx) Chat(ctx context.Context, req llm.ChatRequest, sink *llm.Sink) error {
//		sink.Text("hello")
//		return nil
//	}
//
//	senclaw.Serve(senclaw.Config{
//		Routes: senclaw.MergeRoutes(llm.Routes(&Mlx{}), myRoutes),
//	})
//
// # Why the app owns the wire format and not the provider
//
// A [Provider] emits *semantic* events — visible text, reasoning, a tool call —
// and this package renders them as OpenAI chat.completion.chunk SSE. That split
// is the whole point: the daemon's OpenAI adapter is a real parser with real
// expectations (delta.content, delta.reasoning_content, indexed delta.tool_calls
// whose name and arguments *accumulate* across chunks), and every app that
// hand-rolled that JSON would get a different corner of it wrong. An app that
// implements Provider cannot get it wrong at all.
//
// Because the bytes on the wire are ordinary OpenAI by the time they reach the
// daemon, this reuses adapt: "openai" and needs no new adapter — a local model's
// own tool-call dialect is parsed inside the app, where the model's parser
// config already lives.
package llm

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path"
	"path/filepath"
	"sync/atomic"
)

// ModelsCachePath is where the daemon looks for an app's model list while the
// app is *stopped*, relative to the app's own directory.
//
// A session app is stopped most of the time — that is its resting state — and a
// model nobody can see in the picker is a model nobody selects, calls, or ever
// starts the app for. So [PublishModels] writes the list here at startup and the
// daemon reads it from disk when the process is gone.
const ModelsCachePath = ".senclaw/llm-models.json"

// chunkBuffer bounds the sink so a provider that outruns the client is slowed
// rather than allowed to buffer a whole generation in memory. Mirrors the Rust
// SDK's mpsc channel depth.
const chunkBuffer = 64

// maxBodyBytes caps a chat request body. Multipart image turns are large; 32 MiB
// matches the document ceiling the rest of the SDK uses.
const maxBodyBytes = 32 << 20

// ---------------------------------------------------------------------------
// what a provider advertises
// ---------------------------------------------------------------------------

// ModelCard is one model this app can serve.
//
// Build one with [NewModelCard] rather than a struct literal: the constructor
// sets Tools to true, which is the default the daemon assumes and the value a
// bare struct literal would get wrong (Go's zero bool is false).
type ModelCard struct {
	// ID is the wire id. It is what arrives in [ChatRequest].Model, and what the
	// user sees in the picker unless DisplayName says otherwise.
	ID string `json:"id"`
	// DisplayName is the human label for the picker. Empty means use ID.
	DisplayName string `json:"display_name,omitempty"`
	// ContextLength is the total context window, in tokens.
	ContextLength uint32 `json:"context_length"`
	// MaxOutputTokens is the ceiling on one response, in tokens.
	MaxOutputTokens uint32 `json:"max_output_tokens"`
	// Vision is REQUIRED and never inferred. SenClaw decides whether to send
	// image blocks or fall back to OCR from this field, and the consequences are
	// asymmetric: a text-only endpoint answers an image block with a hard 400
	// that fails the entire turn, while OCR merely degrades it. A local
	// checkpoint is named things like `mlx-community/Qwen3.5-2B-OptiQ-4bit`,
	// which matches no vendor pattern, so a name-based guess lands on the wrong
	// value by accident. The app has the model's config.json open; it knows.
	Vision bool `json:"vision"`
	// Tools reports whether the model can be given tools. Defaults to true;
	// false makes it a chat-only model in the picker. It is always serialized so
	// the cache is explicit, and defaults to true when *absent* on decode — see
	// [ModelCard.UnmarshalJSON].
	Tools bool `json:"tools"`
}

// NewModelCard builds a card with Tools defaulted to true. Chain
// [ModelCard.WithDisplayName] and [ModelCard.WithTools] to set the rest.
func NewModelCard(id string, contextLength, maxOutputTokens uint32, vision bool) ModelCard {
	return ModelCard{
		ID:              id,
		ContextLength:   contextLength,
		MaxOutputTokens: maxOutputTokens,
		Vision:          vision,
		Tools:           true,
	}
}

// WithDisplayName sets the picker label and returns the card, for chaining.
func (m ModelCard) WithDisplayName(name string) ModelCard {
	m.DisplayName = name
	return m
}

// WithTools sets whether the model accepts tools and returns the card.
func (m ModelCard) WithTools(tools bool) ModelCard {
	m.Tools = tools
	return m
}

// UnmarshalJSON decodes a card, defaulting Tools to true when the key is absent.
//
// This is the whole reason the type has a custom decoder: a cached card written
// by an older SDK — or by anything that follows the OpenAI-ish convention of
// omitting a capability that is on — must read back as tool-capable, not as
// chat-only. An explicit `"tools": false` still decodes as false.
func (m *ModelCard) UnmarshalJSON(data []byte) error {
	// alias drops the method set, so decoding into it does not recurse here.
	type alias ModelCard
	aux := alias{Tools: true} // absent "tools" leaves this true
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	*m = ModelCard(aux)
	return nil
}

// ---------------------------------------------------------------------------
// one turn
// ---------------------------------------------------------------------------

// ChatRequest is an incoming turn, in OpenAI chat/completions shape.
//
// The modelled fields are the ones every provider needs. Raw carries the whole
// body besides, because SenClaw sends more than this struct names — HF-style
// tools, stream_options, provider-specific extras — and a provider that
// understands one of them should not have to re-parse to read it.
type ChatRequest struct {
	// Model is which [ModelCard].ID this turn is for.
	Model string
	// Messages are the OpenAI-shaped messages, kept as raw JSON on purpose.
	// content is a string on some turns and an array of parts on others (that is
	// how images arrive), and re-encoding through a typed message would flatten
	// that to a string and drop exactly the parts a vision model needs.
	Messages []json.RawMessage
	// Tools are the OpenAI function definitions, or empty. Raw JSON, same reason.
	Tools []json.RawMessage
	// Stream reports whether the caller asked for SSE. [Handler] serves both, so
	// a provider normally ignores this — it is here for one that can genuinely go
	// faster when nothing is watching.
	Stream bool
	// MaxTokens is the output ceiling for this turn, when the caller set one.
	MaxTokens *uint32
	// Temperature is the sampling temperature, when the caller set one.
	Temperature *float32
	// Raw is the complete request body.
	Raw json.RawMessage
}

// ParseChatRequest parses a chat/completions body.
//
// It reads BOTH spellings of the output ceiling: max_completion_tokens (the
// current one) wins over max_tokens (what older clients, and SenClaw, still
// send). A body with no messages is refused rather than run — an empty turn is a
// bug in the caller, and answering it wastes a whole generation.
func ParseChatRequest(body []byte) (ChatRequest, error) {
	var probe struct {
		Model               string          `json:"model"`
		Messages            json.RawMessage `json:"messages"`
		Tools               json.RawMessage `json:"tools"`
		Stream              bool            `json:"stream"`
		MaxTokens           *uint32         `json:"max_tokens"`
		MaxCompletionTokens *uint32         `json:"max_completion_tokens"`
		Temperature         *float32        `json:"temperature"`
	}
	if err := json.Unmarshal(body, &probe); err != nil {
		return ChatRequest{}, fmt.Errorf("request body is not valid JSON: %w", err)
	}
	if probe.Model == "" {
		return ChatRequest{}, errors.New("`model` is required")
	}

	var messages []json.RawMessage
	if len(probe.Messages) > 0 {
		if err := json.Unmarshal(probe.Messages, &messages); err != nil {
			return ChatRequest{}, errors.New("`messages` must be an array")
		}
	}
	if len(messages) == 0 {
		return ChatRequest{}, errors.New("`messages` must not be empty")
	}

	// tools is best-effort: a present-but-not-an-array value means no tools,
	// matching the Rust `unwrap_or_default`.
	var tools []json.RawMessage
	if len(probe.Tools) > 0 {
		_ = json.Unmarshal(probe.Tools, &tools)
	}

	// max_completion_tokens is the current spelling and wins; max_tokens is the
	// fallback for older clients.
	maxTokens := probe.MaxCompletionTokens
	if maxTokens == nil {
		maxTokens = probe.MaxTokens
	}

	// Copy the body so ChatRequest.Raw never aliases a caller's reused buffer.
	raw := make(json.RawMessage, len(body))
	copy(raw, body)

	return ChatRequest{
		Model:       probe.Model,
		Messages:    messages,
		Tools:       tools,
		Stream:      probe.Stream,
		MaxTokens:   maxTokens,
		Temperature: probe.Temperature,
		Raw:         raw,
	}, nil
}

// ---------------------------------------------------------------------------
// generation events
// ---------------------------------------------------------------------------

type chunkKind int

const (
	kindText chunkKind = iota
	kindReasoning
	kindToolCall
	kindUsage
)

// Chunk is one semantic event from a running generation. Build one with [Text],
// [Reasoning], [ToolCall] or [Usage] — the fields are unexported so a Chunk is
// always a well-formed variant, the way the Rust enum is.
type Chunk struct {
	kind             chunkKind
	text             string
	id               string
	name             string
	arguments        string
	promptTokens     uint64
	completionTokens uint64
}

// Text is visible assistant text, already stripped of any chat-template markers.
func Text(s string) Chunk { return Chunk{kind: kindText, text: s} }

// Reasoning is chain-of-thought, shown separately by SenClaw and echoed back on
// the next request as reasoning_content.
func Reasoning(s string) Chunk { return Chunk{kind: kindReasoning, text: s} }

// ToolCall is a completed tool call. Emit it whole: the SDK renders the
// accumulating delta.tool_calls shape the OpenAI wire requires, so a provider
// never has to stream partial JSON arguments and hope they reassemble.
func ToolCall(id, name, arguments string) Chunk {
	return Chunk{kind: kindToolCall, id: id, name: name, arguments: arguments}
}

// Usage is token counts for this turn. Emit at most once, at the end. SenClaw
// reads it into its usage tracking; omitting it costs only the statistics.
func Usage(promptTokens, completionTokens uint64) Chunk {
	return Chunk{kind: kindUsage, promptTokens: promptTokens, completionTokens: completionTokens}
}

// Sink is the handle a provider writes generation events to.
//
// Sending after the client has disconnected is not an error — it is a no-op, so
// a provider does not need to check. [Sink.IsClosed] is there for one that would
// rather stop generating than finish into a void.
type Sink struct {
	ch   chan Chunk
	done chan struct{}
}

func newSink(buffer int) *Sink {
	return &Sink{ch: make(chan Chunk, buffer), done: make(chan struct{})}
}

// Send writes one event. It blocks only for backpressure while the client is
// still listening, and drops the event once the client has gone.
func (s *Sink) Send(c Chunk) {
	select {
	case s.ch <- c:
	case <-s.done:
		// Consumer gone; dropping is a no-op, exactly like the Rust sink sending
		// into a dropped receiver.
	}
}

// Text is the convenience for the common case.
func (s *Sink) Text(text string) { s.Send(Text(text)) }

// IsClosed reports whether the receiving end has gone away. A provider
// generating a long answer can poll it to abandon a turn no one is listening to.
func (s *Sink) IsClosed() bool {
	select {
	case <-s.done:
		return true
	default:
		return false
	}
}

// Provider is what an app implements to become a model.
type Provider interface {
	// Models returns every model this app can serve, right now.
	Models() []ModelCard

	// Chat runs one turn, writing events to sink as they happen.
	//
	// Returning an error after events have already been sent ends the stream
	// early; the client keeps what it received. Load weights here, lazily — NOT
	// during startup. The daemon health-gates a newly spawned app on a 30-second
	// budget with a 5-second probe timeout, so an app that loads gigabytes before
	// it binds its port is reported as failing to start, with nothing in the
	// error to say that loading was the reason.
	Chat(ctx context.Context, req ChatRequest, sink *Sink) error
}

// ---------------------------------------------------------------------------
// the router
// ---------------------------------------------------------------------------

// Routes returns the two handlers keyed for senclaw.Config.Routes: "GET
// /v1/models" and "POST /v1/chat/completions". Merge them with the app's own via
// senclaw.MergeRoutes and mount at whatever the manifest's llm.path says.
func Routes(provider Provider) map[string]http.Handler {
	return map[string]http.Handler{
		"GET /v1/models": http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			listModels(w, provider)
		}),
		"POST /v1/chat/completions": http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			chatCompletions(w, r, provider)
		}),
	}
}

// Handler is a self-contained mux over the same two routes, for an app that
// serves the LLM endpoints on their own listener or wants them without the
// senclaw.Serve routing layer. It does its own method+path matching rather than
// relying on Go 1.22 ServeMux patterns, so it builds on the module's Go 1.21.
func Handler(provider Provider) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		p := path.Clean("/" + r.URL.Path)
		switch {
		case r.Method == http.MethodGet && p == "/v1/models":
			listModels(w, provider)
		case r.Method == http.MethodPost && p == "/v1/chat/completions":
			chatCompletions(w, r, provider)
		default:
			writeJSON(w, http.StatusNotFound, errorBody("not found: "+p))
		}
	})
}

func listModels(w http.ResponseWriter, provider Provider) {
	cards := provider.Models()
	data := make([]any, 0, len(cards))
	for _, m := range cards {
		data = append(data, map[string]any{
			"id":       m.ID,
			"object":   "model",
			"owned_by": "senclaw-space-app",
			// Not OpenAI fields. The daemon reads them to build the picker entry;
			// another OpenAI client ignores them.
			"display_name":      displayNameOrNull(m),
			"context_length":    m.ContextLength,
			"max_output_tokens": m.MaxOutputTokens,
			"vision":            m.Vision,
			"tools":             m.Tools,
		})
	}
	writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": data})
}

// displayNameOrNull mirrors the Rust router's Option<String>: null when unset,
// so the daemon falls back to the id rather than showing an empty label.
func displayNameOrNull(m ModelCard) any {
	if m.DisplayName == "" {
		return nil
	}
	return m.DisplayName
}

func chatCompletions(w http.ResponseWriter, r *http.Request, provider Provider) {
	body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, maxBodyBytes))
	if err != nil {
		writeJSON(w, http.StatusBadRequest, errorBody("could not read request body: "+err.Error()))
		return
	}
	req, err := ParseChatRequest(body)
	if err != nil {
		writeJSON(w, http.StatusBadRequest, errorBody(err.Error()))
		return
	}
	if !modelKnown(provider, req.Model) {
		// A silent default would answer with the wrong model; 404 says which id
		// was not found.
		writeJSON(w, http.StatusNotFound, errorBody("unknown model `"+req.Model+"`"))
		return
	}
	if req.Stream {
		streamChat(w, r, provider, req)
	} else {
		blockingChat(w, r, provider, req)
	}
}

func modelKnown(provider Provider, model string) bool {
	for _, m := range provider.Models() {
		if m.ID == model {
			return true
		}
	}
	return false
}

// streamChat serves the SSE path.
//
// The status line goes out with the first byte, so a failure partway through
// cannot become a 5xx. It is sent as an error chunk instead — silently ending
// the stream would make a crashed generation look like a short answer, which is
// the one reading a caller cannot recover from. The stream always ends with
// `data: [DONE]`, failure included: leaving it unterminated turns a failed turn
// into a hung one that only the client's read timeout ends.
func streamChat(w http.ResponseWriter, r *http.Request, provider Provider, req ChatRequest) {
	flusher, _ := w.(http.Flusher)
	h := w.Header()
	h.Set("Content-Type", "text/event-stream")
	h.Set("Cache-Control", "no-cache")
	h.Set("Connection", "keep-alive")
	w.WriteHeader(http.StatusOK)

	sink := newSink(chunkBuffer)
	genErr := make(chan error, 1)
	go func() {
		err := safeChat(provider, r.Context(), req, sink)
		genErr <- err
		close(sink.ch) // no more chunks; happens-after every Send inside Chat
	}()
	// Whatever ends this handler, unblock a provider still trying to Send.
	defer close(sink.done)

	id := completionID()
	index := 0
	ctx := r.Context()
consume:
	for {
		select {
		case <-ctx.Done():
			return // client gone
		case c, more := <-sink.ch:
			if !more {
				break consume
			}
			data, emit := chunkToEvent(id, req.Model, c, &index)
			if !emit {
				continue
			}
			if !writeSSE(w, flusher, data) {
				return // write failed — the client hung up
			}
		}
	}

	// sink.ch is closed, so genErr already holds the outcome.
	if err := <-genErr; err != nil {
		writeSSE(w, flusher, errorChunk(id, req.Model, err.Error()))
	}
	writeSSE(w, flusher, "[DONE]")
}

// blockingChat serves the non-stream path: drain the whole generation, then
// answer once. A Chat error becomes 500 rather than a 200 with half an answer —
// a client that gets a truncated body and no explanation cannot tell a short
// reply from a crash.
func blockingChat(w http.ResponseWriter, r *http.Request, provider Provider, req ChatRequest) {
	sink := newSink(chunkBuffer)
	genErr := make(chan error, 1)
	go func() {
		err := safeChat(provider, r.Context(), req, sink)
		genErr <- err
		close(sink.ch)
	}()
	defer close(sink.done)

	var text, reasoning []byte
	var calls []any
	var usage map[string]any
	for c := range sink.ch {
		switch c.kind {
		case kindText:
			text = append(text, c.text...)
		case kindReasoning:
			reasoning = append(reasoning, c.text...)
		case kindToolCall:
			calls = append(calls, map[string]any{
				"id":       c.id,
				"type":     "function",
				"function": map[string]any{"name": c.name, "arguments": c.arguments},
			})
		case kindUsage:
			usage = usageObject(c.promptTokens, c.completionTokens)
		}
	}
	if err := <-genErr; err != nil {
		writeJSON(w, http.StatusInternalServerError, errorBody(err.Error()))
		return
	}
	writeJSON(w, http.StatusOK, nonStreamBody(req.Model, string(text), string(reasoning), calls, usage))
}

// safeChat runs a provider's Chat with a panic guard: a panic in the generation
// goroutine would otherwise take the whole process down, and the agent would see
// a dropped connection rather than a message saying what went wrong.
func safeChat(p Provider, ctx context.Context, req ChatRequest, sink *Sink) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("generation panicked: %v", r)
		}
	}()
	return p.Chat(ctx, req, sink)
}

// chunkToEvent renders one event as the JSON payload of a chat.completion.chunk.
// The bool is false when the event carries nothing to send.
//
// The tool-call shape is the fiddly part and the reason this is not left to
// apps: the consumer accumulates function.name and function.arguments by
// CONCATENATION across chunks at a given index, so a whole call must go out as a
// single delta at a FRESH, incrementing index. Reusing an index silently welds
// two calls into `get_weatherget_time` with both argument objects glued together.
func chunkToEvent(id, model string, c Chunk, index *int) (string, bool) {
	switch c.kind {
	case kindText:
		if c.text == "" {
			return "", false
		}
		return deltaChunk(id, model, map[string]any{"content": c.text}), true
	case kindReasoning:
		if c.text == "" {
			return "", false
		}
		return deltaChunk(id, model, map[string]any{"reasoning_content": c.text}), true
	case kindToolCall:
		i := *index
		*index++
		return deltaChunk(id, model, map[string]any{
			"tool_calls": []any{map[string]any{
				"index":    i,
				"id":       c.id,
				"type":     "function",
				"function": map[string]any{"name": c.name, "arguments": c.arguments},
			}},
		}), true
	case kindUsage:
		// Usage rides its own chunk with an empty choices array — the shape
		// stream_options.include_usage produces, and the one the consumer looks
		// for it in.
		return mustJSON(map[string]any{
			"id":      id,
			"object":  "chat.completion.chunk",
			"model":   model,
			"choices": []any{},
			"usage":   usageObject(c.promptTokens, c.completionTokens),
		}), true
	}
	return "", false
}

// deltaChunk wraps a delta in the standard streaming envelope.
func deltaChunk(id, model string, delta map[string]any) string {
	return mustJSON(map[string]any{
		"id":     id,
		"object": "chat.completion.chunk",
		"model":  model,
		"choices": []any{map[string]any{
			"index":         0,
			"delta":         delta,
			"finish_reason": nil,
		}},
	})
}

// errorChunk is the failure delta sent when generation fails mid-stream.
func errorChunk(id, model, msg string) string {
	return mustJSON(map[string]any{
		"id":     id,
		"object": "chat.completion.chunk",
		"model":  model,
		"choices": []any{map[string]any{
			"index":         0,
			"delta":         map[string]any{},
			"finish_reason": "error",
		}},
		"error": map[string]any{"message": msg, "type": "server_error"},
	})
}

// nonStreamBody assembles the single chat.completion answer for the non-stream
// path. finish_reason is tool_calls when the turn produced calls, else stop;
// reasoning_content is omitted rather than sent blank when there was none.
func nonStreamBody(model, text, reasoning string, calls []any, usage map[string]any) map[string]any {
	message := map[string]any{"role": "assistant", "content": text}
	if reasoning != "" {
		message["reasoning_content"] = reasoning
	}
	if len(calls) > 0 {
		message["tool_calls"] = calls
	}
	finish := "stop"
	if len(calls) > 0 {
		finish = "tool_calls"
	}
	out := map[string]any{
		"id":     completionID(),
		"object": "chat.completion",
		"model":  model,
		"choices": []any{map[string]any{
			"index":         0,
			"message":       message,
			"finish_reason": finish,
		}},
	}
	if usage != nil {
		out["usage"] = usage
	}
	return out
}

func usageObject(prompt, completion uint64) map[string]any {
	return map[string]any{
		"prompt_tokens":     prompt,
		"completion_tokens": completion,
		"total_tokens":      prompt + completion,
	}
}

// writeSSE emits one `data: …` frame and flushes it, so the client sees each
// event as it is produced rather than the whole stream at the end. It reports
// false when the write failed, which is how a hung-up client is detected.
func writeSSE(w io.Writer, flusher http.Flusher, data string) bool {
	if _, err := fmt.Fprintf(w, "data: %s\n\n", data); err != nil {
		return false
	}
	if flusher != nil {
		flusher.Flush()
	}
	return true
}

// completionID is `chatcmpl-<pid><counter>`. Uniqueness only has to hold within
// one client's stream, so process id plus a monotonic counter is enough and
// pulls in no dependency.
var completionCounter atomic.Uint64

func completionID() string {
	return fmt.Sprintf("chatcmpl-%x%x", os.Getpid(), completionCounter.Add(1))
}

func errorBody(msg string) map[string]any {
	return map[string]any{"error": map[string]any{"message": msg, "type": "invalid_request_error"}}
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	raw, err := json.Marshal(v)
	if err != nil {
		raw = []byte(`{"error":{"message":"response is not encodable","type":"server_error"}}`)
		status = http.StatusInternalServerError
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write(raw)
}

func mustJSON(v any) string {
	raw, err := json.Marshal(v)
	if err != nil {
		return `{"error":"chunk is not encodable"}`
	}
	return string(raw)
}

// ---------------------------------------------------------------------------
// model cache
// ---------------------------------------------------------------------------

// PublishModels writes the model list to [ModelsCachePath] under appDir, for the
// daemon to read while this app is stopped. Call it once at startup, after the
// models are known.
//
// An empty list is refused rather than written. The daemon treats a missing
// cache as "not known yet" and a present one as authoritative, so clobbering a
// good list with an empty one during a failed startup would remove the app's
// models from the picker until someone noticed. The write is done to a temp file
// and renamed into place, so a daemon reading concurrently sees either the old
// list or the new one, never a truncated one.
func PublishModels(appDir string, models []ModelCard) error {
	if len(models) == 0 {
		return errors.New("refusing to publish an empty model list")
	}
	target := filepath.Join(appDir, filepath.FromSlash(ModelsCachePath))
	if parent := filepath.Dir(target); parent != "" {
		if err := os.MkdirAll(parent, 0o755); err != nil {
			return err
		}
	}
	raw, err := json.MarshalIndent(map[string]any{"models": models}, "", "  ")
	if err != nil {
		return err
	}
	tmp := target + ".tmp"
	if err := os.WriteFile(tmp, raw, 0o644); err != nil {
		return err
	}
	if err := os.Rename(tmp, target); err != nil {
		_ = os.Remove(tmp)
		return err
	}
	return nil
}
