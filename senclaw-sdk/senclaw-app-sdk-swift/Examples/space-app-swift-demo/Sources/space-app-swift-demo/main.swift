// A complete Space App in Swift, in one file.
//
// What it demonstrates, in the order the daemon exercises it:
//
//  1. `requires.bin: ["swift"]` is checked before this program is ever launched
//     — this demo starts with `swift run`, so the toolchain must be present. A
//     shipped app replaces that with a compiled binary; see README.md.
//  2. There is no install step for a Swift app. The daemon runs `runtime.install`
//     for the node and python runners only, so whatever `start` names must
//     already be runnable.
//  3. `runtime.mode: "session"` — the daemon does not start this at boot. It
//     starts when the user opens the app, or when an agent calls one of the
//     tools below, and stops it again 60 seconds after the last request.
//  4. Two MCP tools (`mcp` block) AND a model this app serves itself (`llm`
//     block) — both are registered into the daemon while the app is stopped, so
//     the first call to either is what starts it.
//
// Run it by hand for development:
//
//     SENCLAW_SPACE_APP_ID=swift-demo PORT=4831 swift run

import Foundation
import SenclawSpace

let appID = "swift-demo"
let startedAt = Date()

// -- an LLM this app serves -------------------------------------------------
//
// A trivial "echo" model, to show the shape. A real provider loads weights
// lazily inside `chat` — never at startup, which would blow the 30s health
// budget — and emits `.reasoning`, `.toolCall` and a final `.usage` alongside
// the visible `.text`.
struct EchoModel: LlmProvider {
    func models() -> [ModelCard] {
        [ModelCard("swift-echo", contextLength: 8192, maxOutputTokens: 512, vision: false,
                   displayName: "Swift Echo")]
    }

    func chat(_ req: ChatRequest, _ sink: ChunkSink) throws {
        let last = req.messages.last as? [String: Any]
        let prompt = (last?["content"] as? String) ?? "(nothing to echo)"
        sink.text("echo: \(prompt)")
        // Provider-reported usage keeps the daemon's accounting whole; a rough
        // chars/4 is fine for a demo.
        sink.send(.usage(promptTokens: prompt.count / 4, completionTokens: prompt.count / 4))
    }
}

// -- MCP tools --------------------------------------------------------------

let mcp = McpServer("swift-demo-mcp")

mcp.tool("swiftdemo_env", "Report the Swift runtime this Space App runs on",
         ["type": "object", "properties": [:]]) { _ in
    [
        "swift": "6.x",
        "platform": "\(ProcessInfo.processInfo.operatingSystemVersionString)",
        "uptimeSecs": Int(Date().timeIntervalSince(startedAt)),
    ]
}

mcp.tool("swiftdemo_uppercase", "Upper-case a piece of text",
         ["type": "object",
          "properties": ["text": ["type": "string", "description": "The text to upper-case"]],
          "required": ["text"]]) { args in
    guard let text = args["text"] as? String, !text.isEmpty else {
        return errorContent("`text` is required")
    }
    return text.uppercased()
}

// -- wire it together -------------------------------------------------------

// The daemon reads the model list from disk while the app is stopped, so a
// session app must publish it at startup.
try? publishModels(FileManager.default.currentDirectoryPath, EchoModel().models())

var routes = llmRoutes(EchoModel())  // GET /v1/models + POST /v1/chat/completions
routes[RouteKey("GET", "/api/status")] = { _ in
    Response(json: ["ok": true, "app": appID, "uptimeSecs": Int(Date().timeIntervalSince(startedAt))])
}

print("[swift-demo] starting")
do {
    try Serve(Config(
        routes: routes,
        healthPath: "/api/status",
        staticDir: "web",
        mcpPath: "/api/mcp/sse",
        mcp: mcp,
        onShutdown: { print("[swift-demo] flushed, bye") },
        defaultPort: 4831
    ))
} catch {
    // The likeliest cause running by hand is the port being in use; a clean line
    // beats Swift's default top-level trap.
    FileHandle.standardError.write(Data("[swift-demo] fatal: \(error)\n".utf8))
    exit(1)
}
