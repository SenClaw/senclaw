// Writing a `senclaw-manifest.json` that says what you meant.
//
// The manifest is what the daemon reads to decide how the app runs, and every
// field it does not understand is silently ignored — a misspelled `mode` makes
// an always-on app on-demand without a word anywhere. So this builds the file
// from named values and validates the ones with a fixed set.
//
// Nothing here is required to write a Space App; the manifest is plain JSON. It
// is here so the fields are discoverable from Swift, and so the bundled
// `senclaw-manifest` executable can check a hand-written file.

import Foundation

public enum RunMode: String { case background, session }
public enum Runner: String { case binary, node, python, shell }
public enum ReadMode: String { case open, strict, allowlist }
public enum NetworkMode: String { case off, all, hosts }

/// The `runtime` block.
///
/// `mode = .background` is for an app that does work nobody asked for at that
/// moment — polls a channel, runs a schedule, holds a WebSocket a browser
/// extension dials into. Everything else is `.session`: started when it is
/// opened or one of its tools is called, stopped once idle.
public func runtimeBlock(
    start: String, port: Int, mode: RunMode = .session, healthPath: String = "/health",
    runner: Runner? = nil, idleTimeoutSecs: Int? = nil, install: String? = nil, venv: Bool? = nil
) -> [String: Any] {
    var block: [String: Any] = [
        "kind": "server", "mode": mode.rawValue, "start": start,
        "healthPath": healthPath, "port": port,
    ]
    if let runner { block["runner"] = runner.rawValue }
    if let idleTimeoutSecs { block["idleTimeoutSecs"] = idleTimeoutSecs }
    if let install { block["install"] = install }
    if let venv { block["venv"] = venv }
    return block
}

/// The `requires` block — what the *machine* must provide.
///
/// Checked at install and again before every launch, so "install ffmpeg" is a
/// sentence the user reads instead of `exit 127` in a log file. Version ranges
/// are the ordinary ones: `>=18`, `>=3.10 <4`, `^18`, `~3.10`, `18.x`.
public func requiresBlock(
    node: String? = nil, python: String? = nil, bin: [String] = [],
    optionalBin: [String] = [], env: [String] = [], os: [String] = []
) -> [String: Any] {
    var out: [String: Any] = [:]
    if let node { out["node"] = node }
    if let python { out["python"] = python }
    if !bin.isEmpty { out["bin"] = bin }
    if !optionalBin.isEmpty { out["optionalBin"] = optionalBin }
    if !env.isEmpty { out["env"] = env }
    if !os.isEmpty { out["os"] = os }
    return out
}

/// The `sandbox` block — the confinement the app asks for itself.
///
/// `force = true` also means the user cannot switch the sandbox off from
/// Plugins → Space Apps, which is the right declaration for an app whose whole
/// point is that it is confined — and the wrong one for an app that merely
/// prefers it. `network = .hosts` needs `hosts`, and it is enforced by an
/// allowlisting proxy: a client that ignores `HTTP_PROXY` reaches *nothing*.
public func sandboxBlock(
    force: Bool = false, enabled: Bool? = nil, readMode: ReadMode? = nil,
    network: NetworkMode? = nil, hosts: [String] = [], daemonApi: Bool? = nil,
    loopback: [Int] = [], folders: [Any] = []
) -> [String: Any] {
    var out: [String: Any] = ["force": force]
    if let enabled { out["enabled"] = enabled }
    if let readMode { out["readMode"] = readMode.rawValue }
    if let network { out["network"] = network.rawValue }
    if !hosts.isEmpty { out["hosts"] = hosts }
    if let daemonApi { out["daemonApi"] = daemonApi }
    if !loopback.isEmpty { out["loopback"] = loopback }
    if !folders.isEmpty { out["folders"] = folders }
    return out
}

/// Assemble a whole manifest dictionary, ready for `manifestJSON`.
public func manifest(
    id: String, name: String, description: String, icon: String = "🦉",
    runtime: [String: Any], mcp: [String: Any]? = nil, requires: [String: Any]? = nil,
    sandbox: [String: Any]? = nil, llm: [String: Any]? = nil, integration: [String: Any]? = nil
) -> [String: Any] {
    var out: [String: Any] = [
        "id": id, "name": name, "description": description, "icon": icon,
        "runtime": runtime,
        "integration": integration ?? ["type": "iframe", "url": "/"],
    ]
    if let mcp { out["mcp"] = mcp }
    if let requires { out["requires"] = requires }
    if let sandbox { out["sandbox"] = sandbox }
    if let llm { out["llm"] = llm }
    return out
}

/// Serialise a manifest dictionary to pretty JSON bytes.
public func manifestJSON(_ m: [String: Any]) throws -> Data {
    try JSONSerialization.data(withJSONObject: m, options: [.prettyPrinted, .sortedKeys])
}

/// Problems found in a manifest, most important first. Empty means the daemon
/// will read what you meant.
public func validateManifest(_ data: [String: Any]) -> [String] {
    var problems: [String] = []
    if (data["id"] as? String)?.isEmpty ?? true { problems.append("missing `id`") }

    if let rt = data["runtime"] as? [String: Any] {
        if (rt["kind"] as? String) == "server", (rt["start"] as? String)?.isEmpty ?? true {
            problems.append("runtime.kind is `server` but there is no `start` command")
        }
        if let mode = rt["mode"] as? String, RunMode(rawValue: mode) == nil {
            problems.append(
                "runtime.mode = \"\(mode)\" is not understood — it is treated as `session`, so an "
                    + "always-on app would silently stop when idle. Use background | session.")
        }
        if let runner = rt["runner"] as? String, Runner(rawValue: runner) == nil {
            problems.append("runtime.runner = \"\(runner)\"; use binary | node | python | shell")
        }
        if let idle = rt["idleTimeoutSecs"] as? Int, idle < 15 {
            problems.append(
                "runtime.idleTimeoutSecs below 15 is clamped to 15 — a shorter window thrashes")
        }
    } else if data["runtime"] != nil {
        problems.append("`runtime` must be an object")
    }

    if let sb = data["sandbox"] as? [String: Any] {
        if (sb["network"] as? String) == "hosts", (sb["hosts"] as? [Any])?.isEmpty ?? true {
            problems.append(
                "sandbox.network is \"hosts\" but `hosts` is empty — the app gets no network")
        }
        if let rm = sb["readMode"] as? String, ReadMode(rawValue: rm) == nil {
            problems.append("sandbox.readMode must be one of open | strict | allowlist")
        }
    }

    if let mcp = data["mcp"] as? [String: Any], (mcp["autoRegister"] as? Bool) == true,
        (mcp["path"] as? String)?.isEmpty ?? true, (mcp["url"] as? String)?.isEmpty ?? true {
        problems.append("mcp.autoRegister is set but there is neither `path` nor `url`")
    }

    // An `llm` block that declares an adapter the daemon does not route means
    // every turn is answered with the wrong wire format — a failure that names
    // neither the app nor the field. Only `openai` and `anthropic` are app-
    // declarable; do not widen this.
    if let llm = data["llm"] as? [String: Any], let adapt = llm["adapt"] as? String,
        adapt != "openai", adapt != "anthropic" {
        problems.append(
            "llm.adapt = \"\(adapt)\" is not app-declarable — use openai | anthropic (an app speaks "
                + "OpenAI and the daemon reuses adapt: \"openai\")")
    }

    return problems
}

/// Load and parse a manifest file. Throws on unreadable file or non-object JSON.
public func loadManifest(_ path: String) throws -> [String: Any] {
    let data = try Data(contentsOf: URL(fileURLWithPath: path))
    guard let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
        throw SenclawError("\(path): top-level JSON is not an object")
    }
    return obj
}
