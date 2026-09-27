package senclaw

// Exposing a Go Space App's tools to SenClaw agents over MCP.
//
// SenClaw's MCP client for an app is deliberately plain: JSON-RPC 2.0 objects
// POSTed to one URL, one request per response, plain application/json back.
// There is no session, no SSE stream and no long-lived connection to manage, so
// the whole server is a map of handlers — which is why this has no dependency
// on an MCP SDK.
//
// Three methods are all a client ever sends:
//
//   - initialize → who you are and what you support
//   - tools/list → the tools, with their JSON Schemas
//   - tools/call → run one
//
// notifications/initialized arrives too and is answered with an empty result;
// SenClaw sends it as a request rather than a notification and ignores the
// reply, but a server that errors on it looks broken in the logs.
//
// The tool names are what agents type, so they follow the repo convention:
// <prefix>_<verb>[_<modifier>] in snake_case, reached by the agent as
// mcp__<mcp.name>__<tool>.

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"runtime/debug"
	"sort"
	"strings"
	"sync"
)

// ProtocolVersion is the MCP revision this server announces.
const ProtocolVersion = "2024-11-05"

// JSON-RPC error codes, as MCP uses them.
const (
	CodeParseError     = -32700
	CodeInvalidRequest = -32600
	CodeMethodNotFound = -32601
	CodeInvalidParams  = -32602
	CodeInternalError  = -32603
)

// Schema is a JSON Schema, written as a literal. It is an alias, so a plain
// map[string]any works everywhere a Schema does.
type Schema = map[string]any

// ToolFunc runs one tool call. The returned value is wrapped by [ToContent]:
// return a string, anything JSON-encodable, or an envelope from [ErrorContent]
// when the failure is one the agent should read and act on.
//
// Returning an error turns into a JSON-RPC error, which the agent sees as a
// broken tool rather than as instructions. Prefer [ErrorContent] for "you
// passed the wrong thing"; reserve errors for "this tool is genuinely broken".
type ToolFunc func(ctx context.Context, args map[string]any) (any, error)

// RPCRequest is one incoming JSON-RPC request.
type RPCRequest struct {
	JSONRPC string          `json:"jsonrpc,omitempty"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// RPCError is a JSON-RPC error object.
type RPCError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

// RPCResponse is one outgoing JSON-RPC response.
type RPCResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Result  any             `json:"result,omitempty"`
	Error   *RPCError       `json:"error,omitempty"`
}

type toolEntry struct {
	Name        string `json:"name"`
	Description string `json:"description"`
	InputSchema Schema `json:"inputSchema"`
	fn          ToolFunc
}

// MCPServer is a registry of tools and the JSON-RPC dispatcher over them. It is
// safe for concurrent use, and implements http.Handler so it can be mounted
// directly at the manifest's mcp.path.
type MCPServer struct {
	Name    string
	Version string

	mu    sync.RWMutex
	order []string
	tools map[string]*toolEntry
}

// NewMCPServer builds a server. name must match the manifest's mcp.name — that
// is the string agents reach the tools through, as mcp__<name>__<tool>.
func NewMCPServer(name, version string) *MCPServer {
	if version == "" {
		version = "1.0.0"
	}
	return &MCPServer{Name: name, Version: version, tools: map[string]*toolEntry{}}
}

// Tool registers one tool.
//
// Write the schema. A tool with no schema is one the model has to guess the
// arguments of, and it guesses badly. Registration order is the order
// tools/list reports, so an agent reads them in the order you wrote them.
func (s *MCPServer) Tool(name, description string, schema Schema, fn ToolFunc) {
	if schema == nil {
		schema = Schema{"type": "object", "properties": Schema{}, "additionalProperties": true}
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, exists := s.tools[name]; !exists {
		s.order = append(s.order, name)
	}
	s.tools[name] = &toolEntry{Name: name, Description: description, InputSchema: schema, fn: fn}
}

// ToolNames returns the registered tool names in registration order.
func (s *MCPServer) ToolNames() []string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return append([]string(nil), s.order...)
}

// Handle answers one JSON-RPC request.
func (s *MCPServer) Handle(ctx context.Context, req *RPCRequest) *RPCResponse {
	resp := &RPCResponse{JSONRPC: "2.0"}
	if req != nil {
		resp.ID = req.ID
	}
	if req == nil {
		resp.Error = &RPCError{Code: CodeInvalidRequest, Message: "empty request"}
		return resp
	}
	result, rpcErr := s.dispatch(ctx, req)
	if rpcErr != nil {
		resp.Error = rpcErr
		return resp
	}
	resp.Result = result
	return resp
}

// HandleJSON answers one JSON-RPC request given its raw body, and returns the
// raw response body. Marshalling never fails in practice; if it somehow does,
// the internal error is reported as JSON-RPC rather than as an empty reply.
func (s *MCPServer) HandleJSON(ctx context.Context, body []byte) []byte {
	var req RPCRequest
	if err := json.Unmarshal(body, &req); err != nil {
		return mustMarshal(&RPCResponse{
			JSONRPC: "2.0",
			Error:   &RPCError{Code: CodeParseError, Message: "parse error: " + err.Error()},
		})
	}
	return mustMarshal(s.Handle(ctx, &req))
}

// ServeHTTP mounts the server at one URL: POST JSON-RPC in, JSON-RPC out.
func (s *MCPServer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		JSON(w, http.StatusMethodNotAllowed, map[string]any{"error": "MCP is POST-only"})
		return
	}
	body, err := readBody(r)
	if err != nil {
		JSON(w, http.StatusBadRequest, map[string]any{"error": err.Error()})
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write(s.HandleJSON(r.Context(), body))
}

func (s *MCPServer) dispatch(ctx context.Context, req *RPCRequest) (any, *RPCError) {
	switch req.Method {
	case "initialize":
		return map[string]any{
			"protocolVersion": ProtocolVersion,
			"capabilities":    map[string]any{"tools": map[string]any{}},
			"serverInfo":      map[string]any{"name": s.Name, "version": s.Version},
		}, nil
	case "notifications/initialized", "initialized", "ping":
		return map[string]any{}, nil
	case "tools/list":
		s.mu.RLock()
		defer s.mu.RUnlock()
		list := make([]*toolEntry, 0, len(s.order))
		for _, name := range s.order {
			list = append(list, s.tools[name])
		}
		return map[string]any{"tools": list}, nil
	case "tools/call":
		var params struct {
			Name      string         `json:"name"`
			Arguments map[string]any `json:"arguments"`
		}
		if len(req.Params) > 0 {
			if err := json.Unmarshal(req.Params, &params); err != nil {
				return nil, &RPCError{Code: CodeInvalidParams, Message: "bad params: " + err.Error()}
			}
		}
		return s.call(ctx, params.Name, params.Arguments)
	default:
		return nil, &RPCError{Code: CodeMethodNotFound, Message: "method not found: " + req.Method}
	}
}

func (s *MCPServer) call(ctx context.Context, name string, args map[string]any) (result any, rpcErr *RPCError) {
	s.mu.RLock()
	tool := s.tools[name]
	known := append([]string(nil), s.order...)
	s.mu.RUnlock()

	if tool == nil {
		sort.Strings(known)
		return nil, &RPCError{
			Code:    CodeInvalidParams,
			Message: fmt.Sprintf("unknown tool: %s (have: %s)", name, strings.Join(known, ", ")),
		}
	}
	if args == nil {
		args = map[string]any{}
	}

	// A panicking tool must not take the whole app down with it: the daemon
	// would restart the process and the agent would see a dropped connection
	// rather than the sentence explaining what went wrong.
	defer func() {
		if r := recover(); r != nil {
			fmt.Printf("[senclaw] tool %s panicked: %v\n%s\n", name, r, debug.Stack())
			result, rpcErr = nil, &RPCError{
				Code:    CodeInternalError,
				Message: fmt.Sprintf("tool %s panicked: %v", name, r),
			}
		}
	}()

	out, err := tool.fn(ctx, args)
	if err != nil {
		return nil, &RPCError{Code: CodeInternalError, Message: err.Error()}
	}
	return ToContent(out), nil
}

// ToContent wraps a tool's return value in the MCP content envelope.
//
// A tool may return a string, anything JSON-encodable, or the envelope itself
// when it wants control. Anything else is JSON-encoded — an agent reads text,
// so returning a bare value that cannot be serialised is a silent nothing.
func ToContent(value any) map[string]any {
	if m, ok := value.(map[string]any); ok {
		if _, hasContent := m["content"]; hasContent {
			return m
		}
	}
	var text string
	switch v := value.(type) {
	case nil:
		text = ""
	case string:
		text = v
	case []byte:
		text = string(v)
	case error:
		text = v.Error()
	case fmt.Stringer:
		text = v.String()
	default:
		raw, err := json.MarshalIndent(value, "", "  ")
		if err != nil {
			text = fmt.Sprintf("%v", value)
		} else {
			text = string(raw)
		}
	}
	return map[string]any{"content": []any{map[string]any{"type": "text", "text": text}}}
}

// ErrorContent is a tool failure the agent can read and act on.
//
// Returned as content with isError, not as a Go error: a JSON-RPC error is a
// transport failure and the agent sees a broken tool, where what it needs is
// the sentence explaining what to do differently.
func ErrorContent(format string, args ...any) map[string]any {
	return map[string]any{
		"content": []any{map[string]any{"type": "text", "text": fmt.Sprintf(format, args...)}},
		"isError": true,
	}
}

func mustMarshal(v any) []byte {
	raw, err := json.Marshal(v)
	if err != nil {
		return []byte(fmt.Sprintf(
			`{"jsonrpc":"2.0","error":{"code":%d,"message":%q}}`,
			CodeInternalError, "reply is not encodable: "+err.Error()))
	}
	return raw
}
