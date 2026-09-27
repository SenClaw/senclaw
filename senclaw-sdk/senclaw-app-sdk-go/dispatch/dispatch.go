// Package dispatch is the app side of SenClaw's autonomous work dispatch.
//
// The daemon's MCPDispatcher engine can drive any app that exposes four
// endpoints. Implement [Provider] over your own store, hand it to [Routes], and
// the engine will claim work from you, keep leases alive, recover items whose
// worker died, and report terminal outcomes back.
//
//	todos := &Todos{db: db}
//	senclaw.Serve(senclaw.Config{
//		Routes: senclaw.MergeRoutes(
//			dispatch.Routes(todos, ""),
//			map[string]http.Handler{"GET /api/status": status},
//		),
//	})
//
// The wire shape is the Rust SDK's, field for field, because the same engine
// parses both: snake_case JSON, Outcome tagged by status, Workspace tagged by
// kind, McpServerSpec tagged by transport. Writing camelCase here is not an
// error — serde ignores the field, and it surfaces as a dependency that never
// held rather than as a failure.
package dispatch

import (
	"context"
	"encoding/json"
	"net/http"
	"strings"
)

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

// Capacity is how many workers the dispatcher can spawn right now.
type Capacity struct {
	// Max items to claim across this source this tick.
	Total int `json:"total"`
	// Max concurrent items per assignee (worker lane). 0 = unlimited.
	PerAssignee int `json:"per_assignee"`
}

// Workspace is where a worker runs. Build one with [Scratch], [Dir] or
// [Worktree] — the engine reads the "kind" tag.
type Workspace map[string]any

// Scratch is a fresh temp dir, discarded when the worker finishes.
func Scratch() Workspace { return Workspace{"kind": "scratch"} }

// Dir is a persistent absolute path.
func Dir(path string) Workspace { return Workspace{"kind": "dir", "path": path} }

// Worktree is a git worktree, for coding tasks. An empty branch means the
// repository's current one.
func Worktree(repo, branch string) Workspace {
	w := Workspace{"kind": "worktree", "repo": repo}
	if branch != "" {
		w["branch"] = branch
	}
	return w
}

// MCPServerSpec is an MCP server the worker should get.
type MCPServerSpec map[string]any

// Stdio is a native stdio MCP server.
//
// Prefer it over [HTTP] — an HTTP spec has to be bridged to stdio by the engine
// at launch, which is one more process and one more failure mode.
func Stdio(name, command string, args []string, env map[string]string) MCPServerSpec {
	if args == nil {
		args = []string{}
	}
	if env == nil {
		env = map[string]string{}
	}
	return MCPServerSpec{"transport": "stdio", "name": name, "command": command,
		"args": args, "env": env}
}

// HTTP is an HTTP/SSE MCP server — e.g. this app's own /api/mcp/sse.
func HTTP(name, url string) MCPServerSpec {
	return MCPServerSpec{"transport": "http", "name": name, "url": url}
}

// WorkItem is a single dispatchable unit of work.
type WorkItem struct {
	// Source-scoped id, opaque to the engine.
	ID string `json:"id"`
	// The task to run — becomes the agent's user prompt.
	Prompt string `json:"prompt"`
	// Worker/persona to route to. Empty = the source's default persona.
	Assignee string `json:"assignee,omitempty"`
	// Extra system-prompt block appended to the persona's own.
	Guidance string `json:"guidance,omitempty"`
	// MCP servers the worker gets, usually including this app's own tools.
	MCP []MCPServerSpec `json:"mcp"`
	// Where the worker runs. Defaults to [Scratch].
	Workspace Workspace `json:"workspace"`
	// Ids this item depends on. Already satisfied by the time you return it.
	DependsOn []string `json:"depends_on"`
	// Higher runs first.
	Priority int `json:"priority"`
	// Per-item run timeout. 0 = the engine's default.
	TimeoutSecs int `json:"timeout_secs,omitempty"`
}

// MarshalJSON fills in the shapes the engine's serde types cannot take null
// for: Vec and the Workspace enum both reject an explicit null, so an item with
// no MCP servers must go out as [] rather than as nothing at all.
func (w WorkItem) MarshalJSON() ([]byte, error) {
	type alias WorkItem // no method set — avoids recursing into this marshaller
	out := alias(w)
	if out.MCP == nil {
		out.MCP = []MCPServerSpec{}
	}
	if out.DependsOn == nil {
		out.DependsOn = []string{}
	}
	if out.Workspace == nil {
		out.Workspace = Scratch()
	}
	return json.Marshal(out)
}

// Outcome is the terminal result of a worker run. Build one with [Completed],
// [Blocked], [Failed] or [TimedOut].
type Outcome map[string]any

// Completed: the work is done.
func Completed(summary string, metadata any) Outcome {
	return Outcome{"status": "completed", "summary": summary, "metadata": metadata}
}

// Blocked: the worker cannot proceed and a human must look.
func Blocked(reason string) Outcome { return Outcome{"status": "blocked", "reason": reason} }

// Failed: the work was attempted and did not succeed.
func Failed(err string) Outcome { return Outcome{"status": "failed", "error": err} }

// TimedOut: the worker ran past its timeout.
func TimedOut() Outcome { return Outcome{"status": "timed_out"} }

// Status reads the outcome's tag: "completed", "blocked", "failed" or
// "timed_out".
func (o Outcome) Status() string {
	s, _ := o["status"].(string)
	return s
}

// ---------------------------------------------------------------------------
// provider
// ---------------------------------------------------------------------------

// Provider is a source of dispatchable work, implemented over your own store.
//
// Embed [Unleased] to get no-op Heartbeat and Reclaim, which are what a source
// with no lease model wants.
type Provider interface {
	// ClaimReady atomically claims up to capacity ready items.
	//
	// Atomically matters: the engine may poll again before these items finish,
	// and an item handed out twice is run twice.
	ClaimReady(ctx context.Context, capacity Capacity) ([]WorkItem, error)

	// Heartbeat extends the lease on an in-flight item.
	Heartbeat(ctx context.Context, itemID string) error

	// Reclaim returns dead-worker/expired-lease items to ready, and reports
	// their ids.
	Reclaim(ctx context.Context) ([]string, error)

	// Finalize records a terminal outcome, mapped to your own states.
	Finalize(ctx context.Context, itemID string, outcome Outcome) error
}

// Unleased provides no-op Heartbeat and Reclaim for a source that has no lease
// model. Embed it in your provider.
type Unleased struct{}

func (Unleased) Heartbeat(context.Context, string) error   { return nil }
func (Unleased) Reclaim(context.Context) ([]string, error) { return []string{}, nil }

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

// DefaultPrefix is where the engine looks unless the source says otherwise.
const DefaultPrefix = "/api/dispatch"

// Routes builds the four handlers, keyed for senclaw.Config.Routes: POST
// {prefix}/poll, /heartbeat, /reclaim and /finalize — the same paths and
// payloads the Rust dispatch_router serves. An empty prefix means
// [DefaultPrefix].
func Routes(p Provider, prefix string) map[string]http.Handler {
	if strings.TrimSpace(prefix) == "" {
		prefix = DefaultPrefix
	}
	prefix = "/" + strings.Trim(prefix, "/")

	return map[string]http.Handler{
		"POST " + prefix + "/poll": handler(func(r *http.Request) (any, error) {
			var body struct {
				Capacity Capacity `json:"capacity"`
			}
			decode(r, &body)
			items, err := p.ClaimReady(r.Context(), body.Capacity)
			if err != nil {
				return nil, err
			}
			if items == nil {
				items = []WorkItem{}
			}
			return items, nil
		}),
		"POST " + prefix + "/heartbeat": handler(func(r *http.Request) (any, error) {
			var body struct {
				ItemID string `json:"item_id"`
			}
			decode(r, &body)
			if err := p.Heartbeat(r.Context(), body.ItemID); err != nil {
				return nil, err
			}
			return map[string]any{"ok": true}, nil
		}),
		"POST " + prefix + "/reclaim": handler(func(r *http.Request) (any, error) {
			ids, err := p.Reclaim(r.Context())
			if err != nil {
				return nil, err
			}
			if ids == nil {
				ids = []string{}
			}
			return ids, nil
		}),
		"POST " + prefix + "/finalize": handler(func(r *http.Request) (any, error) {
			var body struct {
				ItemID  string  `json:"item_id"`
				Outcome Outcome `json:"outcome"`
			}
			decode(r, &body)
			outcome := body.Outcome
			if outcome == nil {
				outcome = Failed("no outcome sent")
			}
			if err := p.Finalize(r.Context(), body.ItemID, outcome); err != nil {
				return nil, err
			}
			return map[string]any{"ok": true}, nil
		}),
	}
}

// handler mirrors senclaw.JSONHandler without importing it: the engine reads
// `error` from a 500 and backs off, so a provider failure must arrive as that
// and not as a dropped connection.
func handler(fn func(*http.Request) (any, error)) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		out, err := fn(r)
		w.Header().Set("Content-Type", "application/json")
		if err != nil {
			w.WriteHeader(http.StatusInternalServerError)
			_ = json.NewEncoder(w).Encode(map[string]any{"error": err.Error()})
			return
		}
		raw, mErr := json.Marshal(out)
		if mErr != nil {
			w.WriteHeader(http.StatusInternalServerError)
			_ = json.NewEncoder(w).Encode(map[string]any{"error": mErr.Error()})
			return
		}
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write(raw)
	})
}

// decode is best-effort: an empty or malformed body leaves the zero value,
// which is what "poll with no capacity" means.
func decode(r *http.Request, dst any) {
	if r.Body == nil {
		return
	}
	_ = json.NewDecoder(r.Body).Decode(dst)
}
