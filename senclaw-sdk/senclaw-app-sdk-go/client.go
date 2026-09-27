// Package senclaw is the SenClaw Space App SDK for Go.
//
// A Space App is an ordinary HTTP server the SenClaw daemon launches,
// health-checks and reverse-proxies. This package is the four things such an
// app needs and nothing else:
//
//   - [Space] — the daemon's API for this app: settings, its own SQLite
//     database, and the AI bridge (the app never holds a provider key).
//   - [MCPServer] — its tools, exposed to agents over MCP.
//   - [Serve] — one HTTP server for health, the UI, the REST API and MCP, with
//     the SIGTERM handling an on-demand app needs.
//   - subpackages manifest and dispatch — writing senclaw-manifest.json, and
//     letting the daemon's dispatcher drive the app.
//
// A minimal app:
//
//	space := senclaw.MustNew()
//	mcp := senclaw.NewMCPServer("demo-mcp", "1.0.0")
//
//	mcp.Tool("demo_greet", "Greet someone", senclaw.Schema{
//		"type":       "object",
//		"properties": senclaw.Schema{"name": senclaw.Schema{"type": "string"}},
//		"required":   []string{"name"},
//	}, func(ctx context.Context, args map[string]any) (any, error) {
//		return "Hello, " + senclaw.String(args, "name"), nil
//	})
//
//	senclaw.Serve(senclaw.Config{
//		Routes:     map[string]http.Handler{"GET /api/status": senclaw.JSONHandler(status)},
//		HealthPath: "/api/status",
//		MCPPath:    "/api/mcp/sse",
//		MCP:        mcp,
//		StaticDir:  "web",
//	})
package senclaw

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"
)

// DefaultBaseURL is the daemon's UI/API server on loopback. Overridden by
// SENCLAW_BASE_URL, which the daemon sets on every launch.
const DefaultBaseURL = "http://127.0.0.1:18788"

// The app's identity to the daemon.
//
// The daemon mints one access token per installed app and puts it in the
// launched process's environment as EnvAppToken. Presenting it on
// /api/space/apps/<id>/… is what tells the daemon *which* app is calling: a
// token is bound to exactly one app id, and using it against another id is
// refused. Without it, every local process that knows an app's id — which is
// public — could read that app's settings, query its database and drive its AI
// bridge.
//
// [Space] sends it on every call automatically. Nothing to do beyond running
// the app through SenClaw.
const (
	// EnvAppToken carries the access token into the app process.
	EnvAppToken = "SENCLAW_TOKEN_ACCESS_APP"
	// EnvAPIVersion carries the Space-App API contract version.
	EnvAPIVersion = "SENCLAW_API_VERSION"
	// HeaderAppToken is where the token travels on a request.
	HeaderAppToken = "X-SenClaw-App-Token"
	// HeaderAPIVersion is where the contract version travels, both directions.
	HeaderAPIVersion = "X-SenClaw-Api-Version"
)

// APIVersion is the Space-App API contract this SDK is written against.
//
// It is sent on every daemon call. A daemon that serves an older contract
// answers 426 rather than half-answering, so an app pinned to a newer SDK than
// its daemon fails at the first call with a message that says which is which.
const APIVersion = 2

// AppTokenFromEnv is the access token the daemon issued this app, or "" when
// the app is running outside SenClaw (a bare `go run .`).
//
// Empty is not an error: a daemon running the default
// SENCLAW_APP_TOKEN_MODE=off serves tokenless calls exactly as it always did.
// Under `strict` those calls are refused, which is the point — see the daemon's
// docs/space-app-api-token.md.
func AppTokenFromEnv() string {
	return strings.TrimSpace(os.Getenv(EnvAppToken))
}

// APIVersionFromEnv is the contract version the daemon launched this app under,
// falling back to [APIVersion] when the app runs outside SenClaw.
func APIVersionFromEnv() int {
	if raw := strings.TrimSpace(os.Getenv(EnvAPIVersion)); raw != "" {
		if n, err := strconv.Atoi(raw); err == nil && n > 0 {
			return n
		}
	}
	return APIVersion
}

// Per-call ceilings, applied only when the caller's context carries no
// deadline of its own. A model call is not a 60-second REST call and an agent
// turn is not a model call.
const (
	DefaultTimeout = 60 * time.Second
	LLMTimeout     = 300 * time.Second
	AgentTimeout   = 900 * time.Second
)

// Error is what every call in this package returns when the daemon says no.
//
// Status is the HTTP status when there was one, and 0 when the request never
// got an answer (nothing listening, timeout) or when the failure was carried
// inside a 200 body — see [Space.Bridge]. Check it with errors.As.
type Error struct {
	Method string
	Path   string
	Status int
	Msg    string
}

func (e *Error) Error() string {
	switch {
	case e.Method == "":
		return e.Msg
	case e.Status == 0:
		return fmt.Sprintf("%s %s → %s", e.Method, e.Path, e.Msg)
	default:
		return fmt.Sprintf("%s %s → HTTP %d: %s", e.Method, e.Path, e.Status, e.Msg)
	}
}

func errf(format string, args ...any) *Error { return &Error{Msg: fmt.Sprintf(format, args...)} }

// StatusOf returns the HTTP status carried by err, or 0 if it carries none.
func StatusOf(err error) int {
	var e *Error
	if errors.As(err, &e) {
		return e.Status
	}
	return 0
}

// AppIDFromEnv is the id the daemon launched this app under, from
// SENCLAW_SPACE_APP_ID.
//
// Falling back to a hard-coded id is fine for local development and wrong in
// production — the id decides which config rows and which database the app
// gets.
func AppIDFromEnv(fallback string) (string, error) {
	if v := strings.TrimSpace(os.Getenv("SENCLAW_SPACE_APP_ID")); v != "" {
		return v, nil
	}
	if fallback != "" {
		return fallback, nil
	}
	return "", errf("SENCLAW_SPACE_APP_ID is not set. Run the app through SenClaw, or pass WithAppID.")
}

// BindHost is the interface this app may listen on.
//
// Loopback unless the operator explicitly opted out. A Space App authenticates
// nothing of its own — the daemon reaches it over 127.0.0.1 and its UI is
// same-origin — so binding 0.0.0.0 hands the whole REST + MCP surface to
// anyone on the network.
func BindHost() string {
	if v := strings.TrimSpace(os.Getenv("SENCLAW_BIND_HOST")); v != "" {
		return v
	}
	return "127.0.0.1"
}

// Port is the port the daemon assigned, from PORT. fallback is used when PORT
// is unset, which only happens when the app is run by hand.
func Port(fallback int) (int, error) {
	if raw := strings.TrimSpace(os.Getenv("PORT")); raw != "" {
		n, err := strconv.Atoi(raw)
		if err == nil && n > 0 && n < 65536 {
			return n, nil
		}
		return 0, errf("PORT=%q is not a port number", raw)
	}
	if fallback > 0 {
		return fallback, nil
	}
	return 0, errf("PORT is not set and no fallback was given")
}

// Space is a client for one Space App's slice of the daemon API.
//
// The zero value is not usable; build one with [New] or [MustNew]. A Space is
// safe for concurrent use.
type Space struct {
	AppID   string
	BaseURL string
	HTTP    *http.Client
	Timeout time.Duration
	// AppToken is this app's access token, from EnvAppToken. Sent on every
	// call; empty when the app runs outside SenClaw.
	AppToken string
	// APIVersion is the contract this client declares. Defaults to the version
	// the daemon launched the app under, else [APIVersion].
	APIVersion int
}

// Option configures a [Space].
type Option func(*Space)

// WithAppID overrides SENCLAW_SPACE_APP_ID. Pass it so a bare `go run .` works
// during development.
func WithAppID(id string) Option { return func(s *Space) { s.AppID = id } }

// WithBaseURL overrides SENCLAW_BASE_URL.
func WithBaseURL(u string) Option { return func(s *Space) { s.BaseURL = u } }

// WithTimeout sets the default per-call ceiling for calls whose context has no
// deadline.
func WithTimeout(d time.Duration) Option { return func(s *Space) { s.Timeout = d } }

// WithHTTPClient supplies the http.Client to use. Give it no Timeout of its
// own — the per-call deadline comes from the context, and a client-level
// timeout would cut a long model call short.
func WithHTTPClient(c *http.Client) Option { return func(s *Space) { s.HTTP = c } }

// WithAppToken overrides the access token from the environment. Pass it when
// running the app by hand against a live daemon — Plugins → Space Apps shows
// the token, or `GET /api/space/apps/<id>/token` returns it.
func WithAppToken(token string) Option {
	return func(s *Space) { s.AppToken = strings.TrimSpace(token) }
}

// WithAPIVersion pins the contract this client declares. Only useful when
// deliberately testing against an older daemon; the default is right.
func WithAPIVersion(v int) Option { return func(s *Space) { s.APIVersion = v } }

// New builds a client from the environment the daemon sets, plus overrides.
func New(opts ...Option) (*Space, error) {
	s := &Space{
		BaseURL:    strings.TrimSpace(os.Getenv("SENCLAW_BASE_URL")),
		Timeout:    DefaultTimeout,
		AppToken:   AppTokenFromEnv(),
		APIVersion: APIVersionFromEnv(),
	}
	for _, o := range opts {
		o(s)
	}
	if s.APIVersion <= 0 {
		s.APIVersion = APIVersion
	}
	if s.AppID == "" {
		id, err := AppIDFromEnv("")
		if err != nil {
			return nil, err
		}
		s.AppID = id
	}
	if s.BaseURL == "" {
		s.BaseURL = DefaultBaseURL
	}
	s.BaseURL = strings.TrimRight(s.BaseURL, "/")
	if s.HTTP == nil {
		s.HTTP = &http.Client{}
	}
	if s.Timeout <= 0 {
		s.Timeout = DefaultTimeout
	}
	return s, nil
}

// MustNew is [New] for a main function: a Space App that cannot resolve its own
// id has nothing useful to do, and failing at the first line beats failing on
// the first tool call.
func MustNew(opts ...Option) *Space {
	s, err := New(opts...)
	if err != nil {
		panic(err)
	}
	return s
}

// ---------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------

func (s *Space) do(ctx context.Context, method, path string, body any, timeout time.Duration) (json.RawMessage, error) {
	if ctx == nil {
		ctx = context.Background()
	}
	if _, hasDeadline := ctx.Deadline(); !hasDeadline {
		if timeout <= 0 {
			timeout = s.Timeout
		}
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, timeout)
		defer cancel()
	}

	var reader io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return nil, &Error{Method: method, Path: path, Msg: "request body is not JSON: " + err.Error()}
		}
		reader = bytes.NewReader(raw)
	}
	req, err := http.NewRequestWithContext(ctx, method, s.BaseURL+path, reader)
	if err != nil {
		return nil, &Error{Method: method, Path: path, Msg: err.Error()}
	}
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	// Who is calling, and under which contract. The token is what lets the
	// daemon scope this call to this app; sending nothing is allowed (and is
	// what happens outside SenClaw) right up until the daemon is set to
	// SENCLAW_APP_TOKEN_MODE=strict.
	if s.AppToken != "" {
		req.Header.Set(HeaderAppToken, s.AppToken)
	}
	if s.APIVersion > 0 {
		req.Header.Set(HeaderAPIVersion, strconv.Itoa(s.APIVersion))
	}

	resp, err := s.HTTP.Do(req)
	if err != nil {
		return nil, &Error{Method: method, Path: path, Msg: err.Error()}
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, &Error{Method: method, Path: path, Status: resp.StatusCode, Msg: err.Error()}
	}
	if resp.StatusCode >= 400 {
		return nil, &Error{Method: method, Path: path, Status: resp.StatusCode, Msg: detail(raw)}
	}
	return json.RawMessage(raw), nil
}

// detail digs the daemon's own message out of an error body, so the caller
// reads "Config key not found" rather than a wall of JSON.
func detail(raw []byte) string {
	var m map[string]any
	if json.Unmarshal(raw, &m) == nil {
		for _, k := range []string{"error", "message", "detail"} {
			if v, ok := m[k].(string); ok && v != "" {
				return v
			}
		}
	}
	s := strings.TrimSpace(string(raw))
	if s == "" {
		return "(empty response)"
	}
	return s
}

func (s *Space) appPath(suffix string) string {
	return "/api/space/apps/" + url.PathEscape(s.AppID) + suffix
}

func decodeObject(raw json.RawMessage) (map[string]any, error) {
	if len(bytes.TrimSpace(raw)) == 0 {
		return map[string]any{}, nil
	}
	var m map[string]any
	if err := json.Unmarshal(raw, &m); err != nil {
		return nil, errf("daemon reply is not a JSON object: %s", truncate(string(raw), 200))
	}
	if m == nil {
		m = map[string]any{}
	}
	return m, nil
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}

// ---------------------------------------------------------------------------
// config KV
// ---------------------------------------------------------------------------

// ConfigItem is one row of [Space.ListConfig].
type ConfigItem struct {
	Key       string          `json:"key"`
	Value     json.RawMessage `json:"value"`
	UpdatedAt int64           `json:"updated_at"`
}

// GetConfig unmarshals one stored setting into out. found is false when the
// key has never been set, and out is left untouched.
//
// This store is shared with the app's own UI, which reads and writes the same
// keys — so this is where settings belong, not in a file inside the app
// directory that an update would overwrite.
func (s *Space) GetConfig(ctx context.Context, key string, out any) (found bool, err error) {
	raw, err := s.do(ctx, http.MethodGet, s.appPath("/config/"+url.PathEscape(key)), nil, 0)
	if err != nil {
		if StatusOf(err) == http.StatusNotFound {
			return false, nil
		}
		return false, err
	}
	var envelope struct {
		Value json.RawMessage `json:"value"`
	}
	if err := json.Unmarshal(raw, &envelope); err != nil || len(envelope.Value) == 0 {
		return false, nil
	}
	if out == nil {
		return true, nil
	}
	if err := json.Unmarshal(envelope.Value, out); err != nil {
		return false, errf("config key %q does not fit %T: %v", key, out, err)
	}
	return true, nil
}

// SetConfig stores one setting. value is JSON-encoded as given.
func (s *Space) SetConfig(ctx context.Context, key string, value any) error {
	_, err := s.do(ctx, http.MethodPut, s.appPath("/config/"+url.PathEscape(key)),
		map[string]any{"value": value}, 0)
	return err
}

// DeleteConfig removes one setting. Deleting a key that was never set is not an
// error.
func (s *Space) DeleteConfig(ctx context.Context, key string) error {
	_, err := s.do(ctx, http.MethodDelete, s.appPath("/config/"+url.PathEscape(key)), nil, 0)
	return err
}

// ListConfig returns every setting this app has stored.
func (s *Space) ListConfig(ctx context.Context) ([]ConfigItem, error) {
	raw, err := s.do(ctx, http.MethodGet, s.appPath("/config"), nil, 0)
	if err != nil {
		return nil, err
	}
	var envelope struct {
		Items []ConfigItem `json:"items"`
	}
	if err := json.Unmarshal(raw, &envelope); err != nil {
		return nil, errf("config list is not the expected shape: %v", err)
	}
	return envelope.Items, nil
}

// ---------------------------------------------------------------------------
// sqlite
// ---------------------------------------------------------------------------

// SQLResult is the answer to one statement. A SELECT (or WITH, or PRAGMA)
// fills Rows; anything else fills RowsAffected and LastInsertRowID.
type SQLResult struct {
	Rows            []map[string]any `json:"rows"`
	RowsAffected    int64            `json:"rowsAffected"`
	LastInsertRowID int64            `json:"lastInsertRowId"`
}

// SQLite runs one statement against this app's own database.
//
// Parameterised: pass values in params, never by formatting them into sql. The
// daemon is the only thing that opens this file, so an injection here is an
// injection into every other app's neighbour.
func (s *Space) SQLite(ctx context.Context, sql string, params ...any) (*SQLResult, error) {
	if params == nil {
		params = []any{}
	}
	raw, err := s.do(ctx, http.MethodPost, s.appPath("/sqlite/query"),
		map[string]any{"sql": sql, "params": params}, 0)
	if err != nil {
		return nil, err
	}
	out := &SQLResult{}
	if err := json.Unmarshal(raw, out); err != nil {
		return nil, errf("sqlite reply is not the expected shape: %v", err)
	}
	return out, nil
}

// SQLiteScan runs a query and unmarshals its rows into dest, which must be a
// pointer to a slice of structs (or of maps).
//
//	var todos []struct {
//		ID    int64  `json:"id"`
//		Title string `json:"title"`
//	}
//	err := space.SQLiteScan(ctx, &todos, "SELECT id, title FROM todos WHERE done = ?", 0)
func (s *Space) SQLiteScan(ctx context.Context, dest any, sql string, params ...any) error {
	res, err := s.SQLite(ctx, sql, params...)
	if err != nil {
		return err
	}
	rows := res.Rows
	if rows == nil {
		rows = []map[string]any{}
	}
	raw, err := json.Marshal(rows)
	if err != nil {
		return errf("sqlite rows are not re-encodable: %v", err)
	}
	if err := json.Unmarshal(raw, dest); err != nil {
		return errf("sqlite rows do not fit %T: %v", dest, err)
	}
	return nil
}

// ---------------------------------------------------------------------------
// the AI bridge
// ---------------------------------------------------------------------------

// Bridge calls one of the daemon's bridge actions.
//
// The generic form. Prefer the named wrappers below, which document the traps
// in each.
//
// Two things this handles that a hand-rolled POST does not:
//
// The wire field is "action". The daemon's request struct requires it and
// defines no alias, so any other spelling is a 422 before a line of handler
// code runs — which reads as "the bridge is down" rather than "you sent the
// wrong key".
//
// A failed bridge action comes back as HTTP 200 carrying
// {"status":"error","message":…} — the transport worked, the action did not.
// Checking only the HTTP code turns a dead provider into an empty string,
// which reads downstream as "the model had nothing to say".
func (s *Space) Bridge(ctx context.Context, action string, payload map[string]any, timeout time.Duration) (map[string]any, error) {
	if payload == nil {
		payload = map[string]any{}
	}
	raw, err := s.do(ctx, http.MethodPost, s.appPath("/bridge"),
		map[string]any{"action": action, "payload": payload}, timeout)
	if err != nil {
		return nil, err
	}
	result, err := decodeObject(raw)
	if err != nil {
		return nil, err
	}
	if status, ok := result["status"].(string); ok && status != "ok" {
		if status == "pending" {
			return nil, errf("bridge action %q is not enabled in this daemon", action)
		}
		if msg, ok := result["message"].(string); ok && msg != "" {
			return nil, errf("%s", msg)
		}
		return nil, errf("bridge action %q failed", action)
	}
	return result, nil
}

// Capabilities reports what this daemon's bridge actually supports, asked of
// the daemon rather than assumed.
func (s *Space) Capabilities(ctx context.Context) ([]string, error) {
	result, err := s.Bridge(ctx, "capabilities", nil, 0)
	if err != nil {
		return nil, err
	}
	list, _ := result["capabilities"].([]any)
	out := make([]string, 0, len(list))
	for _, c := range list {
		if s, ok := c.(string); ok {
			out = append(out, s)
		}
	}
	return out, nil
}

// LLMRequest is one model call.
//
// Only System, Prompt, MaxTokens and Profile are read — there is no
// temperature knob, and passing one is silently ignored rather than honoured.
type LLMRequest struct {
	Prompt string
	System string
	// Defaults to 4000. Watch it: a reply that hits the ceiling comes back
	// truncated with Finish == "length".
	MaxTokens int
	// A named model profile. Use this rather than [Space.SetActiveModel] when
	// the app wants its own model — the active model is global.
	Profile string
}

func (r LLMRequest) payload() map[string]any {
	max := r.MaxTokens
	if max <= 0 {
		max = 4000
	}
	p := map[string]any{"prompt": r.Prompt, "maxTokens": max}
	if r.System != "" {
		p["system"] = r.System
	}
	if r.Profile != "" {
		p["profile"] = r.Profile
	}
	return p
}

// LLMUsage is provider-reported token usage for one model call.
//
// InputTokens is the TOTAL billed input — cache tokens included, not on top
// of. The two cache fields break it down for providers that report them
// (Anthropic); adding them to InputTokens double-counts.
type LLMUsage struct {
	InputTokens         int64 `json:"inputTokens"`
	OutputTokens        int64 `json:"outputTokens"`
	CacheReadTokens     int64 `json:"cacheReadTokens"`
	CacheCreationTokens int64 `json:"cacheCreationTokens"`
}

// LLMReply is the full reply shape from [Space.LLMDetailed].
type LLMReply struct {
	Text  string `json:"text"`
	Model string `json:"model"`
	// "length" (hit the token cap), "stop", or "" when unreported.
	Finish string `json:"finish"`
	// nil when the provider reported no usage — unknown, not zero.
	Usage *LLMUsage `json:"usage"`
}

// LLM makes one model call through the user's configured provider and returns
// the text.
//
// It returns an error when the reply was truncated at MaxTokens: a cut-off
// paragraph is indistinguishable from a short answer, and silently returning
// one is how half an answer ends up saved as the whole thing. Chunk long work
// rather than raising the ceiling, or use [Space.LLMDetailed] to handle the
// truncation yourself.
func (s *Space) LLM(ctx context.Context, req LLMRequest) (string, error) {
	reply, err := s.LLMDetailed(ctx, req)
	if err != nil {
		return "", err
	}
	if reply.Finish == "length" {
		return "", errf("the model hit maxTokens and the reply is truncated — " +
			"split the work into smaller chunks rather than raising the ceiling")
	}
	return reply.Text, nil
}

// LLMDetailed is the same call as [Space.LLM], returning everything the
// provider said.
//
// Use it when you want to handle a truncated reply instead of having it
// returned as an error, or when you need real token counts. Usage is nil when
// the provider reported none — some local models do — which means unknown, not
// zero.
func (s *Space) LLMDetailed(ctx context.Context, req LLMRequest) (LLMReply, error) {
	result, err := s.Bridge(ctx, "llm.request", req.payload(), LLMTimeout)
	if err != nil {
		return LLMReply{}, err
	}
	out := LLMReply{
		Text:   str(result["text"]),
		Model:  str(result["model"]),
		Finish: str(result["finish"]),
	}
	if out.Text == "" {
		out.Text = str(result["content"])
	}
	if raw, ok := result["usage"].(map[string]any); ok {
		out.Usage = &LLMUsage{
			InputTokens:         num(raw["inputTokens"]),
			OutputTokens:        num(raw["outputTokens"]),
			CacheReadTokens:     num(raw["cacheReadTokens"]),
			CacheCreationTokens: num(raw["cacheCreationTokens"]),
		}
	}
	return out, nil
}

// Agent runs a full agent turn — tools, multiple steps, the lot.
//
// Slower and far more capable than [Space.LLM]. Use it when the work needs the
// agent's tools; use LLM when it needs a paragraph of text. tools, when given,
// restricts the roster to those tool names.
func (s *Space) Agent(ctx context.Context, prompt string, tools ...string) (map[string]any, error) {
	payload := map[string]any{"prompt": prompt}
	if len(tools) > 0 {
		payload["tools"] = tools
	}
	return s.Bridge(ctx, "agent.run", payload, AgentTimeout)
}

// ---------------------------------------------------------------------------
// knowledge
//
// Each space is an independent memory partition. Leaving Space empty uses the
// app's own private one, named after the app id — so an app that never names a
// space can neither read nor pollute anybody else's memory.
// ---------------------------------------------------------------------------

// Memory is one thing to remember.
type Memory struct {
	Text   string
	Space  string
	Source string
	Tags   []string
}

// KnowledgeHit is one hit from [Space.KnowledgeSearch].
type KnowledgeHit struct {
	Name    string  `json:"name"`
	Summary string  `json:"summary"`
	Score   float64 `json:"score"`
}

// KnowledgeSave saves one memory into a knowledge space.
func (s *Space) KnowledgeSave(ctx context.Context, m Memory) error {
	payload := map[string]any{"text": m.Text}
	if m.Space != "" {
		payload["space"] = m.Space
	}
	if m.Source != "" {
		payload["source"] = m.Source
	}
	if len(m.Tags) > 0 {
		payload["tags"] = m.Tags
	}
	_, err := s.Bridge(ctx, "knowledge.save", payload, 0)
	return err
}

// KnowledgeSearch is a scoped search over one knowledge space — raw hits, no
// synthesis. limit <= 0 means the daemon's default.
func (s *Space) KnowledgeSearch(ctx context.Context, query, space string, limit int) ([]KnowledgeHit, error) {
	payload := map[string]any{"query": query}
	if limit > 0 {
		payload["limit"] = limit
	}
	if space != "" {
		payload["space"] = space
	}
	result, err := s.Bridge(ctx, "knowledge.search", payload, 0)
	if err != nil {
		return nil, err
	}
	raw, err := json.Marshal(result["hits"])
	if err != nil {
		return nil, errf("knowledge hits are not re-encodable: %v", err)
	}
	var hits []KnowledgeHit
	if err := json.Unmarshal(raw, &hits); err != nil {
		return nil, errf("knowledge hits are not the expected shape: %v", err)
	}
	return hits, nil
}

// RecallQuery is a scoped recall with LLM synthesis.
type RecallQuery struct {
	Query string
	Space string
	Limit int
	// How far to walk the knowledge graph. 0 means the daemon's default.
	Hops int
}

// KnowledgeRecall answers a question from one knowledge space — one synthesised
// answer, not a hit list.
//
// An empty string means the space held nothing relevant. That is a real answer,
// not an error.
func (s *Space) KnowledgeRecall(ctx context.Context, q RecallQuery) (string, error) {
	payload := map[string]any{"query": q.Query}
	if q.Space != "" {
		payload["space"] = q.Space
	}
	if q.Limit > 0 {
		payload["limit"] = q.Limit
	}
	if q.Hops > 0 {
		payload["hops"] = q.Hops
	}
	result, err := s.Bridge(ctx, "knowledge.recall", payload, LLMTimeout)
	if err != nil {
		return "", err
	}
	return str(result["answer"]), nil
}

// ---------------------------------------------------------------------------
// accounting & models
// ---------------------------------------------------------------------------

// Usage is one directly-billed model call to report.
type Usage struct {
	Model        string
	Provider     string
	InputTokens  int64
	OutputTokens int64
	LatencyMs    int64
	// True when the numbers are chars/4 guesses rather than provider-reported.
	Estimated bool
}

// UsageReport reports tokens for a call the app made directly to a provider.
//
// Only for apps holding their own API key and bypassing [Space.LLM] — it keeps
// the daemon's accounting whole. Fire-and-forget by design: it returns nothing,
// because a failure here must never take down the work it describes.
func (s *Space) UsageReport(ctx context.Context, u Usage) {
	_, _ = s.Bridge(ctx, "usage.report", map[string]any{
		"model":        u.Model,
		"provider":     u.Provider,
		"inputTokens":  u.InputTokens,
		"outputTokens": u.OutputTokens,
		"latencyMs":    u.LatencyMs,
		"estimated":    u.Estimated,
	}, 0)
}

// ModelInfo is one LLM configured in the daemon.
type ModelInfo struct {
	ID        string `json:"id"`
	ModelName string `json:"modelName"`
	Provider  string `json:"provider"`
	Adapt     string `json:"adapt"`
}

// ListModels returns the daemon's configured LLMs and the id of the active one.
func (s *Space) ListModels(ctx context.Context) (active string, models []ModelInfo, err error) {
	raw, err := s.Core(ctx, http.MethodGet, "/api/llm-config", nil)
	if err != nil {
		return "", nil, err
	}
	var envelope struct {
		ActiveID string      `json:"activeId"`
		Configs  []ModelInfo `json:"configs"`
	}
	if err := json.Unmarshal(raw, &envelope); err != nil {
		return "", nil, errf("llm-config is not the expected shape: %v", err)
	}
	for _, c := range envelope.Configs {
		// An entry with no id is not a model — it cannot be selected and
		// cannot be reported against.
		if c.ID == "" {
			continue
		}
		if c.Provider == "" {
			c.Provider = c.Adapt
		}
		models = append(models, c)
	}
	return envelope.ActiveID, models, nil
}

// SetActiveModel switches the daemon's active main model.
//
// Global — the agent and every other app share it. An app that wants its own
// model should set LLMRequest.Profile rather than moving everyone else's
// cheese.
func (s *Space) SetActiveModel(ctx context.Context, modelID string) error {
	_, err := s.Core(ctx, http.MethodPost, "/api/llm-config/active", map[string]any{"id": modelID})
	return err
}

// ---------------------------------------------------------------------------
// everything else
// ---------------------------------------------------------------------------

// MCPRegistration registers an MCP server with the daemon on this app's behalf.
//
// Transport is "stdio", "sse" or "http", plus the fields that transport needs.
// Most apps never call this: declaring mcp.autoRegister in the manifest is the
// normal route, and it survives a restart.
type MCPRegistration struct {
	Name        string            `json:"name,omitempty"`
	Transport   string            `json:"transport"`
	Description string            `json:"description,omitempty"`
	URL         string            `json:"url,omitempty"`
	Command     string            `json:"command,omitempty"`
	Args        []string          `json:"args,omitempty"`
	Env         map[string]string `json:"env,omitempty"`
	Headers     map[string]string `json:"headers,omitempty"`
	UseTools    []string          `json:"use_tools,omitempty"`
	Enabled     *bool             `json:"enabled,omitempty"`
}

// RegisterMCP registers an MCP server with the daemon on this app's behalf.
func (s *Space) RegisterMCP(ctx context.Context, reg MCPRegistration) (map[string]any, error) {
	raw, err := s.do(ctx, http.MethodPost, s.appPath("/mcp/register"), reg, 0)
	if err != nil {
		return nil, err
	}
	return decodeObject(raw)
}

// Core calls any other daemon endpoint, e.g.
// Core(ctx, "GET", "/api/wiki/list", nil).
func (s *Space) Core(ctx context.Context, method, path string, body any) (json.RawMessage, error) {
	if !strings.HasPrefix(path, "/") {
		path = "/" + path
	}
	return s.do(ctx, method, path, body, 0)
}

// ---------------------------------------------------------------------------
// small helpers for the untyped side of JSON
// ---------------------------------------------------------------------------

func str(v any) string {
	s, _ := v.(string)
	return s
}

func num(v any) int64 {
	switch n := v.(type) {
	case float64:
		return int64(n)
	case int64:
		return n
	case int:
		return int64(n)
	case json.Number:
		i, _ := n.Int64()
		return i
	}
	return 0
}

// String reads a string argument out of a decoded JSON object, which is what a
// tool handler is handed. Missing or wrong-typed reads as "".
func String(args map[string]any, key string) string { return str(args[key]) }

// Int reads a number argument out of a decoded JSON object. Missing or
// wrong-typed reads as 0 — JSON numbers arrive as float64, which is the trap
// this exists for.
func Int(args map[string]any, key string) int { return int(num(args[key])) }

// Bool reads a boolean argument out of a decoded JSON object.
func Bool(args map[string]any, key string) bool {
	b, _ := args[key].(bool)
	return b
}
