// `senclaw-manifest <senclaw-manifest.json>` — the Swift twin of
// `python -m senclaw_space.manifest` and the Node `senclaw-manifest` bin.
//
// Worth having as a binary rather than only a function: the manifest mistakes
// that matter are the silent ones (a misspelled `runtime.mode` falls back to
// `session`, so an app that must poll a channel quietly stops after a minute),
// and a check you can run in CI without writing a script is a check that gets
// run.
//
//     swift run senclaw-manifest senclaw-manifest.json

import Foundation
import SenclawSpace

let files = CommandLine.arguments.dropFirst().filter { !$0.hasPrefix("-") }
if files.isEmpty {
    FileHandle.standardError.write(Data("usage: senclaw-manifest <senclaw-manifest.json> [...]\n".utf8))
    exit(2)
}

var failed = 0
for file in files {
    do {
        let m = try loadManifest(file)
        let problems = validateManifest(m)
        if problems.isEmpty {
            let rt = (m["runtime"] as? [String: Any]) ?? [:]
            let mode = (rt["mode"] as? String) ?? "session"
            let runner = (rt["runner"] as? String) ?? "auto"
            print("✓ \((m["id"] as? String) ?? "?"): mode=\(mode) runner=\(runner)")
        } else {
            failed += 1
            for p in problems { print("✗ \(file): \(p)") }
        }
    } catch {
        failed += 1
        print("✗ \(file): \(error)")
    }
}

exit(failed > 0 ? 1 : 0)
