package manifest

// The manifest validator earns its place on exactly one axis: catching the
// spellings the daemon accepts silently. Everything checked here fails with no
// error message anywhere if it is not checked here.

import (
	"encoding/json"
	"os"
	"strings"
	"testing"
)

func writeFile(path, body string) error { return os.WriteFile(path, []byte(body), 0o644) }

func problemsContaining(t *testing.T, m *Manifest, want ...string) {
	t.Helper()
	problems := strings.Join(Validate(m), "\n")
	for _, w := range want {
		if !strings.Contains(problems, w) {
			t.Fatalf("problems = %q, want one mentioning %q", problems, w)
		}
	}
}

func TestAMisspelledModeIsCaughtBecauseNothingElseCatchesIt(t *testing.T) {
	bad := &Manifest{ID: "x", Runtime: &Runtime{Kind: "server", Start: "./x", Mode: "backgroud"}}
	problemsContaining(t, bad, "mode", "session")

	good := &Manifest{ID: "x", Runtime: &Runtime{Kind: "server", Start: "./x", Mode: ModeBackground}}
	if p := Validate(good); len(p) != 0 {
		t.Fatalf("a correct manifest has no problems: %v", p)
	}
}

func TestAnEmptyHostAllowlistMeansNoNetworkAtAll(t *testing.T) {
	bad := &Manifest{ID: "x", Sandbox: &Sandbox{Network: NetworkHosts}}
	problemsContaining(t, bad, "no network")
}

func TestAServerAppWithoutAStartCommandIsFlagged(t *testing.T) {
	problemsContaining(t, &Manifest{ID: "x", Runtime: &Runtime{Kind: "server"}}, "start")
}

func TestAnIdleTimeoutBelowTheFloorIsFlagged(t *testing.T) {
	m := &Manifest{ID: "x", Runtime: &Runtime{Kind: "server", Start: "./x", IdleTimeoutSecs: 5}}
	problemsContaining(t, m, "clamped to 15")
}

func TestInstallIsFlaggedForACompiledApp(t *testing.T) {
	// The trap this package exists for, on the Go side: the daemon runs the
	// install command for node and python runners only, so `go build` declared
	// here never happens and `start` points at a binary nobody built.
	m := &Manifest{ID: "x", Runtime: &Runtime{
		Kind: "server", Start: "./demo", Install: "go build -o demo .",
	}}
	problemsContaining(t, m, "never executes")

	// Declared on a node app, it is exactly right and must not be flagged.
	ok := &Manifest{ID: "x", Runtime: &Runtime{
		Kind: "server", Start: "npm start", Install: "npm ci",
	}}
	if p := Validate(ok); len(p) != 0 {
		t.Fatalf("node install flagged: %v", p)
	}
}

func TestAutoRegisterWithoutAnEndpoint(t *testing.T) {
	m := &Manifest{ID: "x", MCP: &MCP{Name: "x-mcp", AutoRegister: true}}
	problemsContaining(t, m, "neither `path` nor `url`")
}

func TestInferRunnerMirrorsTheDaemon(t *testing.T) {
	cases := map[string]Runner{
		"./crm":            RunnerBinary,
		"npm start":        RunnerNode,
		"node server.js":   RunnerNode,
		"server.mjs":       RunnerNode,
		"python3 -m app":   RunnerPython,
		"main.py":          RunnerPython,
		"uv run app.py":    RunnerPython,
		"caddy run":        RunnerShell,
		"go run .":         RunnerShell,
		"./space-app-demo": RunnerBinary,
	}
	for start, want := range cases {
		if got := InferRunner(start); got != want {
			t.Errorf("InferRunner(%q) = %q, want %q", start, got, want)
		}
	}
}

func TestDefineRefusesToProduceABadManifest(t *testing.T) {
	_, err := Define(Manifest{ID: "x", Runtime: &Runtime{Kind: "server", Start: "./x", Mode: "backgroud"}})
	if err == nil {
		t.Fatal("Define must fail where the manifest is written, not at install")
	}
}

func TestTheTypesProduceWhatTheDaemonReads(t *testing.T) {
	enabled := true
	m, err := Define(Manifest{
		ID:          "go-demo",
		Name:        "Go Demo",
		Description: "A demo",
		Icon:        "🐹",
		Runtime: &Runtime{
			Kind: "server", Mode: ModeSession, Start: "./go-demo",
			HealthPath: "/api/status", Port: 4820, IdleTimeoutSecs: 60,
		},
		Requires: &Requires{Bin: []string{"ffmpeg"}, OS: []string{"macos", "linux"}},
		Sandbox: &Sandbox{
			Force: true, Enabled: &enabled, Network: NetworkHosts, Hosts: []string{"api.openai.com"},
		},
		MCP: &MCP{Name: "go-demo-mcp", Transport: "http", Path: "/api/mcp/sse", AutoRegister: true},
	})
	if err != nil {
		t.Fatalf("Define: %v", err)
	}
	raw, err := m.JSON()
	if err != nil {
		t.Fatalf("JSON: %v", err)
	}
	var back map[string]any
	if err := json.Unmarshal(raw, &back); err != nil {
		t.Fatal(err)
	}
	rt := back["runtime"].(map[string]any)
	if rt["mode"] != "session" || rt["idleTimeoutSecs"] != float64(60) {
		t.Fatalf("runtime = %v", rt)
	}
	if back["sandbox"].(map[string]any)["force"] != true {
		t.Fatalf("sandbox = %v", back["sandbox"])
	}
	// Absent optional blocks must not appear as nulls — the daemon reads a
	// `"requires": null` differently from no requires at all.
	if _, present := back["bridge"]; present {
		t.Fatalf("an undeclared block was serialised: %s", raw)
	}
}

func TestLoadKeepsFieldsThisPackageDoesNotModel(t *testing.T) {
	// A manifest may carry blocks the SDK has no type for; Load must not lose
	// them just because Validate does not look at them.
	dir := t.TempDir() + "/senclaw-manifest.json"
	body := `{"id":"x","name":"X","futureBlock":{"a":1}}`
	if err := writeFile(dir, body); err != nil {
		t.Fatal(err)
	}
	m, err := Load(dir)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if m.ID != "x" {
		t.Fatalf("id = %q", m.ID)
	}
	if _, ok := m.Extra["futureBlock"]; !ok {
		t.Fatalf("Extra = %v", m.Extra)
	}
}
