// Command senclaw-manifest checks a senclaw-manifest.json for the mistakes the
// daemon accepts silently.
//
//	go run github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/cmd/senclaw-manifest senclaw-manifest.json
//
// Exit code 1 means problems were found, 2 means the file could not be read.
package main

import (
	"fmt"
	"os"

	"github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/manifest"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: senclaw-manifest <senclaw-manifest.json>")
		os.Exit(2)
	}
	m, err := manifest.Load(os.Args[1])
	if err != nil {
		fmt.Fprintln(os.Stderr, "✗ "+err.Error())
		os.Exit(2)
	}
	problems := manifest.Validate(m)
	for _, p := range problems {
		fmt.Println("✗ " + p)
	}
	if len(problems) > 0 {
		os.Exit(1)
	}
	mode, runner := manifest.ModeSession, manifest.Runner("auto")
	if m.Runtime != nil {
		if m.Runtime.Mode != "" {
			mode = m.Runtime.Mode
		}
		if m.Runtime.Runner != "" {
			runner = m.Runtime.Runner
		} else if m.Runtime.Start != "" {
			runner = manifest.InferRunner(m.Runtime.Start) + " (inferred)"
		}
	}
	fmt.Printf("✓ %s: mode=%s runner=%s\n", m.ID, mode, runner)
}
