// Package manifest writes and checks a senclaw-manifest.json that says what
// you meant.
//
// The manifest is what the daemon reads to decide how the app runs, and every
// field it does not understand is silently ignored — a misspelled mode makes an
// always-on app on-demand without a word anywhere. So the fields are typed
// here, and [Validate] checks the values that have a fixed set.
//
// Nothing here is required to write a Space App. It exists so the fields are
// discoverable from Go, and so `senclaw-manifest` can check a hand-written
// file:
//
//	go run github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/cmd/senclaw-manifest senclaw-manifest.json
package manifest

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"
)

// RunMode is runtime.mode.
type RunMode string

// Runner is runtime.runner — what kind of program runtime.start is.
type Runner string

// ReadMode is sandbox.readMode.
type ReadMode string

// NetworkMode is sandbox.network.
type NetworkMode string

const (
	// ModeBackground starts with the daemon and is supervised: for an app that
	// does work nobody asked for at that moment — polls a channel, runs a
	// schedule, holds a WebSocket a browser extension dials into.
	ModeBackground RunMode = "background"
	// ModeSession is the default: started when the app is opened or one of its
	// MCP tools is called, stopped once idle.
	ModeSession RunMode = "session"

	RunnerBinary Runner = "binary"
	RunnerNode   Runner = "node"
	RunnerPython Runner = "python"
	RunnerShell  Runner = "shell"

	ReadOpen      ReadMode = "open"
	ReadStrict    ReadMode = "strict"
	ReadAllowlist ReadMode = "allowlist"

	NetworkOff   NetworkMode = "off"
	NetworkAll   NetworkMode = "all"
	NetworkHosts NetworkMode = "hosts"
)

var (
	validModes     = []RunMode{ModeBackground, ModeSession}
	validRunners   = []Runner{RunnerBinary, RunnerNode, RunnerPython, RunnerShell}
	validReadModes = []ReadMode{ReadOpen, ReadStrict, ReadAllowlist}
	validNetworks  = []NetworkMode{NetworkOff, NetworkAll, NetworkHosts}
)

// Runtime is the runtime block: how the daemon starts and stops the app.
type Runtime struct {
	// "server" is the only shape the daemon launches anything for.
	Kind string `json:"kind,omitempty"`
	// Empty means session — the default, and rarely wrong.
	Mode       RunMode `json:"mode,omitempty"`
	Start      string  `json:"start,omitempty"`
	Port       int     `json:"port,omitempty"`
	HealthPath string  `json:"healthPath,omitempty"`
	// Empty means inferred from Start: `./app` → binary, `npm …` → node,
	// `python …` → python, anything else → shell.
	Runner Runner `json:"runner,omitempty"`
	// Session apps only. 60s by default, 15s floor.
	IdleTimeoutSecs int `json:"idleTimeoutSecs,omitempty"`
	// Run once after install/update, before the first launch.
	//
	// Only node and python runners run it. A Go app is a binary or shell
	// runner, so declaring an install command here is silently skipped — see
	// [Validate].
	Install string `json:"install,omitempty"`
	Venv    *bool  `json:"venv,omitempty"`
}

// Requires is what the machine must provide, checked at install and again
// before every launch — so "install ffmpeg" is a sentence the user reads
// instead of exit 127 in a log file.
type Requires struct {
	// A range: >=18, ^18, 18.x, >=18 <21.
	Node        string   `json:"node,omitempty"`
	Python      string   `json:"python,omitempty"`
	Bin         []string `json:"bin,omitempty"`
	OptionalBin []string `json:"optionalBin,omitempty"`
	Env         []string `json:"env,omitempty"`
	OptionalEnv []string `json:"optionalEnv,omitempty"`
	// "macos", "linux", "windows".
	OS []string `json:"os,omitempty"`
}

// Folder is one path granted to a sandboxed app.
type Folder struct {
	Path     string `json:"path"`
	ReadOnly bool   `json:"readOnly,omitempty"`
}

// Sandbox is the confinement the app asks for itself, applied at install.
type Sandbox struct {
	// The user may not turn the sandbox off in Plugins → Space Apps. The right
	// declaration for an app whose whole point is that it is confined, and the
	// wrong one for an app that merely prefers it.
	Force    bool     `json:"force,omitempty"`
	Enabled  *bool    `json:"enabled,omitempty"`
	ReadMode ReadMode `json:"readMode,omitempty"`
	// "hosts" is enforced by an allowlisting proxy rather than an OS rule: no
	// sandbox here can filter by hostname. A client that ignores HTTP_PROXY
	// therefore reaches nothing, not everything — test with it on before
	// shipping the declaration.
	Network   NetworkMode `json:"network,omitempty"`
	Hosts     []string    `json:"hosts,omitempty"`
	DaemonAPI *bool       `json:"daemonApi,omitempty"`
	Loopback  []int       `json:"loopback,omitempty"`
	Folders   []Folder    `json:"folders,omitempty"`
}

// MCP is the mcp block. Name is what agents reach the tools through:
// mcp__<name>__<tool>.
type MCP struct {
	Name         string `json:"name,omitempty"`
	Transport    string `json:"transport,omitempty"`
	Path         string `json:"path,omitempty"`
	URL          string `json:"url,omitempty"`
	Description  string `json:"description,omitempty"`
	AutoRegister bool   `json:"autoRegister,omitempty"`
}

// Manifest is senclaw-manifest.json.
type Manifest struct {
	ID          string         `json:"id"`
	Name        string         `json:"name"`
	Description string         `json:"description,omitempty"`
	Icon        string         `json:"icon,omitempty"`
	Version     string         `json:"version,omitempty"`
	Runtime     *Runtime       `json:"runtime,omitempty"`
	Requires    *Requires      `json:"requires,omitempty"`
	Sandbox     *Sandbox       `json:"sandbox,omitempty"`
	Integration map[string]any `json:"integration,omitempty"`
	Bridge      map[string]any `json:"bridge,omitempty"`
	MCP         *MCP           `json:"mcp,omitempty"`
	// Extra holds anything not modelled above. It is not merged into the
	// output by [Manifest.JSON] — write those fields in the file by hand.
	Extra map[string]any `json:"-"`
}

// Load reads and parses a manifest file.
func Load(path string) (*Manifest, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	m := &Manifest{}
	if err := json.Unmarshal(raw, m); err != nil {
		return nil, fmt.Errorf("%s is not valid JSON: %w", path, err)
	}
	_ = json.Unmarshal(raw, &m.Extra)
	return m, nil
}

// JSON renders the manifest as the daemon reads it.
func (m *Manifest) JSON() ([]byte, error) { return json.MarshalIndent(m, "", "  ") }

// Define validates at authoring time and returns the manifest unchanged, so a
// bad declaration fails where it was written rather than at install.
func Define(m Manifest) (Manifest, error) {
	if problems := Validate(&m); len(problems) > 0 {
		return m, fmt.Errorf("invalid senclaw-manifest:\n  - %s", strings.Join(problems, "\n  - "))
	}
	return m, nil
}

// Validate returns the problems in a manifest, most important first. An empty
// result means the daemon will read what you meant.
func Validate(m *Manifest) []string {
	var problems []string
	if m == nil {
		return []string{"manifest is nil"}
	}
	if strings.TrimSpace(m.ID) == "" {
		problems = append(problems, "missing `id`")
	}
	if rt := m.Runtime; rt != nil {
		if rt.Kind == "server" && strings.TrimSpace(rt.Start) == "" {
			problems = append(problems, "runtime.kind is `server` but there is no `start` command")
		}
		if rt.Mode != "" && !contains(validModes, rt.Mode) {
			problems = append(problems, fmt.Sprintf(
				"runtime.mode = %q is not understood — it is treated as `session`, so an "+
					"always-on app would silently stop when idle. Use one of %s.",
				rt.Mode, join(validModes)))
		}
		if rt.Runner != "" && !contains(validRunners, rt.Runner) {
			problems = append(problems, fmt.Sprintf(
				"runtime.runner = %q; use one of %s", rt.Runner, join(validRunners)))
		}
		if rt.IdleTimeoutSecs != 0 && rt.IdleTimeoutSecs < 15 {
			problems = append(problems,
				"runtime.idleTimeoutSecs below 15 is clamped to 15 — a shorter window thrashes")
		}
		// The trap that bites every compiled-language app: the daemon runs
		// `install` for node and python runners only (src/apps/prepare.rs
		// returns early for binary and shell), so a Go app that expects
		// `go build` to happen at install time ships a manifest whose start
		// command points at a binary nobody built.
		if rt.Install != "" {
			runner := rt.Runner
			if runner == "" {
				runner = InferRunner(rt.Start)
			}
			if runner == RunnerBinary || runner == RunnerShell {
				problems = append(problems, fmt.Sprintf(
					"runtime.install is set but runner is %q — the daemon only runs install for "+
						"`node` and `python`, so this command never executes. Ship the built "+
						"program in the app directory, or build it in `start`.", runner))
			}
		}
	}
	if sb := m.Sandbox; sb != nil {
		if sb.Network == NetworkHosts && len(sb.Hosts) == 0 {
			problems = append(problems,
				`sandbox.network is "hosts" but `+"`hosts`"+` is empty — the app gets no network`)
		}
		if sb.Network != "" && !contains(validNetworks, sb.Network) {
			problems = append(problems, fmt.Sprintf("sandbox.network must be one of %s", join(validNetworks)))
		}
		if sb.ReadMode != "" && !contains(validReadModes, sb.ReadMode) {
			problems = append(problems, fmt.Sprintf("sandbox.readMode must be one of %s", join(validReadModes)))
		}
	}
	if mcp := m.MCP; mcp != nil && mcp.AutoRegister && mcp.Path == "" && mcp.URL == "" {
		problems = append(problems, "mcp.autoRegister is set but there is neither `path` nor `url`")
	}
	return problems
}

// InferRunner mirrors the daemon's guess when runtime.runner is absent
// (src/apps/manifest.rs). Only the unambiguous cases are claimed; everything
// else is shell.
func InferRunner(start string) Runner {
	s := strings.TrimSpace(start)
	first := s
	if i := strings.IndexAny(s, " \t"); i >= 0 {
		first = s[:i]
	}
	base := first
	if i := strings.LastIndexAny(first, `/\`); i >= 0 {
		base = first[i+1:]
	}
	switch base {
	case "node", "npm", "npx", "pnpm", "yarn", "bun", "deno":
		return RunnerNode
	case "python", "python3", "py", "uv", "poetry", "pipenv":
		return RunnerPython
	}
	switch {
	case strings.HasSuffix(base, ".js"), strings.HasSuffix(base, ".mjs"), strings.HasSuffix(base, ".cjs"):
		return RunnerNode
	case strings.HasSuffix(base, ".py"):
		return RunnerPython
	case strings.HasPrefix(s, "./"), strings.HasPrefix(s, `.\`):
		return RunnerBinary
	default:
		return RunnerShell
	}
}

func contains[T comparable](list []T, v T) bool {
	for _, item := range list {
		if item == v {
			return true
		}
	}
	return false
}

func join[T ~string](list []T) string {
	parts := make([]string, len(list))
	for i, v := range list {
		parts[i] = string(v)
	}
	return strings.Join(parts, " | ")
}
