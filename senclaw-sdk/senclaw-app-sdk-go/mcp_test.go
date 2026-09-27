package senclaw

// Two things are worth pinning: that the MCP dispatcher answers exactly what
// SenClaw's Rust client sends, and that a broken tool degrades into a message
// rather than into a dead app — both fail invisibly otherwise.

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
)

func buildMCP() *MCPServer {
	s := NewMCPServer("demo-mcp", "2.0.0")
	s.Tool("demo_echo", "Echo", Schema{
		"type":       "object",
		"properties": Schema{"text": Schema{"type": "string"}},
	}, func(_ context.Context, args map[string]any) (any, error) {
		return "you said " + String(args, "text"), nil
	})
	s.Tool("demo_add", "Add two numbers", nil,
		func(_ context.Context, args map[string]any) (any, error) {
			return map[string]any{"sum": Int(args, "a") + Int(args, "b")}, nil
		})
	return s
}

func call(t *testing.T, s *MCPServer, body string) map[string]any {
	t.Helper()
	var out map[string]any
	if err := json.Unmarshal(s.HandleJSON(context.Background(), []byte(body)), &out); err != nil {
		t.Fatalf("reply is not JSON: %v", err)
	}
	return out
}

func TestTheThreeMethodsSenclawActuallySends(t *testing.T) {
	s := buildMCP()

	init := call(t, s, `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}`)
	if init["id"] != float64(1) {
		t.Fatalf("id = %v", init["id"])
	}
	result := init["result"].(map[string]any)
	info := result["serverInfo"].(map[string]any)
	if info["name"] != "demo-mcp" || info["version"] != "2.0.0" {
		t.Fatalf("serverInfo = %v", info)
	}
	if _, ok := result["capabilities"].(map[string]any)["tools"]; !ok {
		t.Fatal("a server with no advertised tools capability lists nothing")
	}

	// SenClaw sends this as a request with an id, not a notification, and
	// ignores the reply — but a server that errors on it looks broken in logs.
	note := call(t, s, `{"jsonrpc":"2.0","id":2,"method":"notifications/initialized"}`)
	if _, bad := note["error"]; bad {
		t.Fatalf("notifications/initialized errored: %v", note["error"])
	}

	listed := call(t, s, `{"jsonrpc":"2.0","id":3,"method":"tools/list"}`)
	tools := listed["result"].(map[string]any)["tools"].([]any)
	var names []string
	for _, tool := range tools {
		names = append(names, tool.(map[string]any)["name"].(string))
	}
	// Registration order, not map order: an agent reads them as written.
	if strings.Join(names, ",") != "demo_echo,demo_add" {
		t.Fatalf("tools = %v", names)
	}
	// The schema must survive: a tool with none is one the model guesses at.
	schema := tools[0].(map[string]any)["inputSchema"].(map[string]any)
	props := schema["properties"].(map[string]any)["text"].(map[string]any)
	if props["type"] != "string" {
		t.Fatalf("schema = %v", schema)
	}
	if _, leaked := tools[0].(map[string]any)["fn"]; leaked {
		t.Fatal("never serialise the handler")
	}
}

func TestToolsCall(t *testing.T) {
	s := buildMCP()
	r := call(t, s, `{"id":1,"method":"tools/call","params":{"name":"demo_echo","arguments":{"text":"hi"}}}`)
	content := r["result"].(map[string]any)["content"].([]any)
	if content[0].(map[string]any)["text"] != "you said hi" {
		t.Fatalf("content = %v", content)
	}

	r = call(t, s, `{"id":2,"method":"tools/call","params":{"name":"demo_add","arguments":{"a":2,"b":3}}}`)
	text := r["result"].(map[string]any)["content"].([]any)[0].(map[string]any)["text"].(string)
	var sum map[string]any
	if err := json.Unmarshal([]byte(text), &sum); err != nil {
		t.Fatalf("a struct return must arrive as JSON text: %v", err)
	}
	if sum["sum"] != float64(5) {
		t.Fatalf("sum = %v — JSON numbers arrive as float64", sum)
	}
}

func TestAnUnknownToolNamesTheOnesThatExist(t *testing.T) {
	r := call(t, buildMCP(), `{"id":1,"method":"tools/call","params":{"name":"nope","arguments":{}}}`)
	msg := r["error"].(map[string]any)["message"].(string)
	if !strings.Contains(msg, "demo_echo") {
		t.Fatalf("message = %q — say what does exist", msg)
	}
}

func TestAFailingToolBecomesAnErrorNotADeadServer(t *testing.T) {
	s := NewMCPServer("x", "")
	s.Tool("x_boom", "Always fails", nil, func(context.Context, map[string]any) (any, error) {
		return nil, errf("nope")
	})
	s.Tool("x_panic", "Panics", nil, func(_ context.Context, args map[string]any) (any, error) {
		// The realistic shape: an unchecked type assertion on an argument the
		// agent did not send.
		return args["missing"].(string), nil
	})

	r := call(t, s, `{"id":1,"method":"tools/call","params":{"name":"x_boom"}}`)
	e := r["error"].(map[string]any)
	if e["code"] != float64(CodeInternalError) || !strings.Contains(e["message"].(string), "nope") {
		t.Fatalf("error = %v", e)
	}

	// A panic in one tool must not take the process down: the daemon would
	// restart it and the agent would see a dropped connection instead of a
	// sentence.
	r = call(t, s, `{"id":2,"method":"tools/call","params":{"name":"x_panic"}}`)
	if !strings.Contains(r["error"].(map[string]any)["message"].(string), "panicked") {
		t.Fatalf("error = %v", r["error"])
	}
}

func TestUnknownMethodAndUnparseableBody(t *testing.T) {
	s := buildMCP()
	r := call(t, s, `{"id":1,"method":"tools/delete"}`)
	if r["error"].(map[string]any)["code"] != float64(CodeMethodNotFound) {
		t.Fatalf("error = %v", r["error"])
	}
	r = call(t, s, `{not json`)
	if r["error"].(map[string]any)["code"] != float64(CodeParseError) {
		t.Fatalf("error = %v", r["error"])
	}
}

func TestContentEnvelope(t *testing.T) {
	if ToContent("hi")["content"].([]any)[0].(map[string]any)["text"] != "hi" {
		t.Fatal("a string is its own text")
	}
	text := ToContent(map[string]any{"a": 1})["content"].([]any)[0].(map[string]any)["text"].(string)
	if !strings.Contains(text, `"a": 1`) {
		t.Fatalf("text = %q", text)
	}
	// An envelope passes through untouched, so a tool can control its shape.
	envelope := map[string]any{"content": []any{}, "isError": true}
	if got := ToContent(envelope); got["isError"] != true {
		t.Fatalf("envelope = %v", got)
	}
	if ErrorContent("try %s", "again")["isError"] != true {
		t.Fatal("ErrorContent must mark itself as one")
	}
}
