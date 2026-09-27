// Talking to the SenClaw daemon from a Swift Space App.
//
// Everything an app is allowed to do beyond its own process goes through the
// daemon over loopback: storing settings, querying its own SQLite database, and
// — the important one — asking a model anything. An app never holds a provider
// API key; it calls the bridge and the daemon uses the user's configured
// provider.
//
// Foundation only, on purpose. A Space App with no third-party dependencies has
// no `swift package resolve` step to fail, so the daemon's prepare step is a
// no-op and the app starts as fast as the binary loads.

import Foundation

#if canImport(FoundationNetworking)
// On Linux the URLSession types live in a separate module; on Apple platforms
// they are already in Foundation. Importing both keeps one source file building
// everywhere.
import FoundationNetworking
#endif

/// Env var carrying this app's access token into its process, set by the daemon
/// on every launch.
///
/// The daemon mints one token per installed app. Presenting it on
/// `/api/space/apps/<id>/…` is what tells the daemon *which* app is calling: a
/// token is bound to one app id, and using it against another is refused.
/// Without it, any local process that knows an app's id — which is public —
/// could read that app's settings, query its database and drive its AI bridge.
public let ENV_APP_TOKEN = "SENCLAW_TOKEN_ACCESS_APP"

/// Env var carrying the Space-App API contract version.
public let ENV_API_VERSION = "SENCLAW_API_VERSION"

/// Header the access token travels in.
public let HEADER_APP_TOKEN = "X-SenClaw-App-Token"

/// Header the contract version travels in, both directions.
public let HEADER_API_VERSION = "X-SenClaw-Api-Version"

/// The Space-App API contract this SDK is written against. Sent on every call;
/// a daemon serving an older contract answers 426 rather than half-answering.
public let API_VERSION = 2

/// The daemon's UI/API server on loopback. Overridden by `SENCLAW_BASE_URL`,
/// which the daemon sets on every launch.
public let DEFAULT_BASE_URL = "http://127.0.0.1:18788"

// ---------------------------------------------------------------------------
// environment
// ---------------------------------------------------------------------------

/// The id the daemon launched this app under, from `SENCLAW_SPACE_APP_ID`.
///
/// Falling back to a hard-coded id is fine for local development and wrong in
/// production — the id decides which config rows and which database the app
/// gets.
public func appIdFromEnv(_ fallback: String? = nil) throws -> String {
    let value = ProcessInfo.processInfo.environment["SENCLAW_SPACE_APP_ID"]?
        .trimmingCharacters(in: .whitespaces)
    if let value, !value.isEmpty { return value }
    if let fallback, !fallback.isEmpty { return fallback }
    throw SenclawError(
        "SENCLAW_SPACE_APP_ID is not set. Run the app through SenClaw, or pass "
            + "appId when constructing SpaceClient.")
}

/// The interface an app may listen on.
///
/// Loopback unless the operator explicitly opted out. A Space App authenticates
/// nothing of its own — the daemon reaches it over 127.0.0.1 and its UI is
/// same-origin — so binding `0.0.0.0` hands the whole REST + MCP surface to
/// anyone on the network.
public func bindHost() -> String {
    let v = ProcessInfo.processInfo.environment["SENCLAW_BIND_HOST"]?
        .trimmingCharacters(in: .whitespaces)
    if let v, !v.isEmpty { return v }
    return "127.0.0.1"
}

/// The access token the daemon issued this app, or `""` outside SenClaw.
///
/// Empty is not an error: a daemon on the default `SENCLAW_APP_TOKEN_MODE=off`
/// serves tokenless calls exactly as it always did. Under `strict` they are
/// refused — which is the point.
public func appTokenFromEnv() -> String {
    ProcessInfo.processInfo.environment[ENV_APP_TOKEN]?
        .trimmingCharacters(in: .whitespaces) ?? ""
}

/// The contract version the daemon launched this app under.
public func apiVersionFromEnv() -> Int {
    if let raw = ProcessInfo.processInfo.environment[ENV_API_VERSION]?
        .trimmingCharacters(in: .whitespaces), let n = Int(raw), n > 0 {
        return n
    }
    return API_VERSION
}

/// The port the daemon assigned, from `PORT`. `fallback` is used when `PORT` is
/// unset, which only happens when the app is run by hand.
public func appPort(_ fallback: Int = 0) throws -> Int {
    if let raw = ProcessInfo.processInfo.environment["PORT"]?
        .trimmingCharacters(in: .whitespaces), !raw.isEmpty {
        if let n = Int(raw), n > 0, n < 65536 { return n }
        throw SenclawError("PORT=\(raw) is not a port number")
    }
    if fallback > 0 { return fallback }
    throw SenclawError("PORT is not set and no fallback was given")
}

// ---------------------------------------------------------------------------
// error
// ---------------------------------------------------------------------------

/// What every call in this package throws when the daemon says no.
///
/// `status` is the HTTP status when there was one, and `nil` when the request
/// never got an answer (nothing listening, timeout) or when the failure was
/// carried inside a 200 body — see `bridge`.
public struct SenclawError: Error, CustomStringConvertible {
    public let message: String
    public let status: Int?

    public init(_ message: String, status: Int? = nil) {
        self.message = message
        self.status = status
    }

    public var description: String { message }
}

// ---------------------------------------------------------------------------
// bridge reply types
// ---------------------------------------------------------------------------

/// Provider-reported token usage for one `llm.request`.
///
/// `inputTokens` is the TOTAL billed input — cache tokens included, not on top
/// of. The two cache fields break it down for providers that report them
/// (Anthropic); adding them to `inputTokens` double-counts.
public struct LlmUsage: Equatable {
    public var inputTokens: Int = 0
    public var outputTokens: Int = 0
    public var cacheReadTokens: Int = 0
    public var cacheCreationTokens: Int = 0
}

/// The full reply shape from `llmDetailed`.
public struct LlmReply {
    public let text: String
    public let model: String
    /// `"length"` (hit the token cap), `"stop"`, or `""` when unreported.
    public let finish: String
    /// `nil` when the provider reported no usage — unknown, not zero.
    public let usage: LlmUsage?
}

/// One hit from `knowledgeSearch`.
public struct KnowledgeHit {
    public let name: String
    public let summary: String
    public let score: Double
}

/// One LLM configured in the daemon.
public struct ModelInfo {
    public let id: String
    public let modelName: String?
    public let provider: String?
}

// ---------------------------------------------------------------------------
// client
// ---------------------------------------------------------------------------

/// A client for one Space App's slice of the daemon API.
///
/// Safe to share across threads: `URLSession` is, and this holds no mutable
/// state. Every method blocks until the daemon answers — a Space App's request
/// handlers run on their own connection thread, so a synchronous client is the
/// natural fit and there is no async to thread through a tool callback.
public final class SpaceClient {
    public let appId: String
    public let baseURL: String
    /// Sent on every call. Empty when the app runs outside SenClaw.
    public let appToken: String
    /// The contract this client declares. Defaults to the version the daemon
    /// launched the app under, else `API_VERSION`.
    public let apiVersion: Int
    /// Default per-call ceiling for calls that pass no timeout of their own.
    public let timeout: TimeInterval

    private let session: URLSession

    public init(
        appId: String? = nil,
        baseURL: String? = nil,
        appToken: String? = nil,
        apiVersion: Int? = nil,
        timeout: TimeInterval = 60
    ) throws {
        self.appId = try (appId ?? appIdFromEnv())
        let resolved = baseURL
            ?? ProcessInfo.processInfo.environment["SENCLAW_BASE_URL"]
            ?? DEFAULT_BASE_URL
        self.baseURL = resolved.hasSuffix("/") ? String(resolved.dropLast()) : resolved
        self.appToken = (appToken ?? appTokenFromEnv()).trimmingCharacters(in: .whitespaces)
        self.apiVersion = apiVersion ?? apiVersionFromEnv()
        self.timeout = timeout
        // No session-wide timeout: the per-call deadline is applied on the
        // request, and a session timeout would cut a long model call short.
        self.session = URLSession(configuration: .ephemeral)
    }

    // -- plumbing ---------------------------------------------------------

    /// One request/response, blocking. Returns the parsed JSON body (a
    /// dictionary, array, scalar) or `nil` when the body was empty.
    @discardableResult
    func request(
        _ method: String,
        _ path: String,
        body: Any? = nil,
        timeout: TimeInterval? = nil
    ) throws -> Any? {
        guard let url = URL(string: baseURL + path) else {
            throw SenclawError("bad url: \(baseURL + path)")
        }
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.timeoutInterval = timeout ?? self.timeout
        req.setValue("application/json", forHTTPHeaderField: "Accept")
        if let body {
            req.httpBody = try JSONSerialization.data(withJSONObject: body)
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        // Who is calling, and under which contract. An empty token is omitted
        // rather than sent blank: the daemon would try to resolve "" and refuse
        // a call that its default mode would have served.
        if !appToken.isEmpty { req.setValue(appToken, forHTTPHeaderField: HEADER_APP_TOKEN) }
        if apiVersion > 0 {
            req.setValue(String(apiVersion), forHTTPHeaderField: HEADER_API_VERSION)
        }

        let (data, response) = try blockingSend(req)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        if status >= 400 {
            throw SenclawError("\(method) \(path) → HTTP \(status): \(detail(data))", status: status)
        }
        guard let data, !data.isEmpty else { return nil }
        return try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
    }

    /// Bridge the completion-handler API to a blocking call. The wait happens
    /// after the handler stored its result, so the two never touch concurrently.
    private func blockingSend(_ req: URLRequest) throws -> (Data?, URLResponse?) {
        let box = ResultBox()
        let sem = DispatchSemaphore(value: 0)
        let task = session.dataTask(with: req) { data, response, error in
            box.data = data
            box.response = response
            box.error = error
            sem.signal()
        }
        task.resume()
        sem.wait()
        if let error = box.error {
            throw SenclawError("\(req.httpMethod ?? "GET") \(req.url?.path ?? "") → \(error.localizedDescription)")
        }
        return (box.data, box.response)
    }

    private func appPath(_ suffix: String) -> String {
        "/api/space/apps/\(pathEscape(appId))\(suffix)"
    }

    // -- config KV --------------------------------------------------------

    /// One stored setting, or `nil` when it has never been set.
    ///
    /// Shared with the app's own UI, which reads and writes the same keys — so
    /// this is where settings belong, not in a file inside the app directory
    /// that an update would overwrite.
    public func getConfig(_ key: String) throws -> Any? {
        do {
            let payload = try request("GET", appPath("/config/\(pathEscape(key))"))
            if let dict = payload as? [String: Any] { return dict["value"] }
            return payload
        } catch let e as SenclawError where e.status == 404 {
            return nil
        }
    }

    @discardableResult
    public func setConfig(_ key: String, _ value: Any) throws -> Any? {
        let payload = try request("PUT", appPath("/config/\(pathEscape(key))"), body: ["value": value])
        if let dict = payload as? [String: Any] { return dict["value"] }
        return payload
    }

    public func deleteConfig(_ key: String) throws {
        _ = try request("DELETE", appPath("/config/\(pathEscape(key))"))
    }

    /// Every setting this app has stored.
    public func listConfig() throws -> [[String: Any]] {
        let payload = try request("GET", appPath("/config"))
        if let dict = payload as? [String: Any], let items = dict["items"] as? [[String: Any]] {
            return items
        }
        return []
    }

    // -- sqlite -----------------------------------------------------------

    /// Run one statement against this app's own database.
    ///
    /// Parameterised: pass values in `params`, never by formatting them into
    /// `sql`. The daemon is the only thing that opens this file, so an injection
    /// here is an injection into every other app's neighbour.
    @discardableResult
    public func sqlite(_ sql: String, _ params: [Any] = []) throws -> [String: Any] {
        let payload = try request("POST", appPath("/sqlite/query"), body: ["sql": sql, "params": params])
        return (payload as? [String: Any]) ?? [:]
    }

    // -- the AI bridge ----------------------------------------------------

    /// Call one of the daemon's bridge actions.
    ///
    /// The generic form. Prefer the named wrappers below, which document the
    /// traps in each.
    ///
    /// The wire field is `action`. The daemon's request struct requires it and
    /// defines no alias, so any other spelling is a 422 before a line of handler
    /// code runs — which reads as "the bridge is down" rather than "you sent the
    /// wrong key".
    ///
    /// A failed bridge action comes back as HTTP 200 carrying
    /// `{"status":"error","message":…}` — the transport worked, the action did
    /// not. Checking only the HTTP code turns a dead provider into an empty
    /// string, which reads downstream as "the model had nothing to say".
    @discardableResult
    public func bridge(_ action: String, _ payload: [String: Any] = [:], timeout: TimeInterval? = nil) throws -> [String: Any] {
        let result = try request(
            "POST", appPath("/bridge"),
            body: ["action": action, "payload": payload], timeout: timeout)
        let dict = (result as? [String: Any]) ?? [:]
        if let status = dict["status"] as? String, status != "ok" {
            if status == "pending" {
                throw SenclawError("bridge action '\(action)' is not enabled in this daemon")
            }
            throw SenclawError((dict["message"] as? String) ?? "bridge action '\(action)' failed")
        }
        return dict
    }

    /// What this daemon's bridge actually supports, asked of the daemon.
    public func capabilities() throws -> [String] {
        let r = try bridge("capabilities", [:])
        return (r["capabilities"] as? [Any])?.compactMap { $0 as? String } ?? []
    }

    /// One model call, through the user's configured provider.
    ///
    /// Only `system`, `prompt`, `maxTokens` and `profile` are read — there is no
    /// temperature knob, and passing one is silently ignored rather than
    /// honoured.
    ///
    /// Watch `maxTokens`: a reply that hits the ceiling comes back truncated with
    /// `finish == "length"`, which reads as a model that gave a short answer
    /// rather than as an error. This throws on that instead of returning the
    /// fragment — chunk long work rather than raising the ceiling and hoping.
    public func llm(
        prompt: String,
        system: String? = nil,
        maxTokens: Int = 4000,
        profile: String? = nil,
        timeout: TimeInterval = 300
    ) throws -> String {
        let reply = try llmDetailed(
            prompt: prompt, system: system, maxTokens: maxTokens, profile: profile, timeout: timeout)
        if reply.finish == "length" {
            throw SenclawError(
                "the model hit maxTokens and the reply is truncated — split the work into "
                    + "smaller chunks rather than raising the ceiling")
        }
        return reply.text
    }

    /// The same call as `llm`, returning everything the provider said.
    ///
    /// Use it when you want to *handle* a truncated reply instead of having it
    /// thrown at you (`finish == "length"` means the cap was hit), or when you
    /// need real token counts. `usage` is `nil` when the provider reported none
    /// — some local models do — which means unknown, not zero.
    public func llmDetailed(
        prompt: String,
        system: String? = nil,
        maxTokens: Int = 4000,
        profile: String? = nil,
        timeout: TimeInterval = 300
    ) throws -> LlmReply {
        var payload: [String: Any] = ["prompt": prompt, "maxTokens": maxTokens]
        if let system, !system.isEmpty { payload["system"] = system }
        if let profile, !profile.isEmpty { payload["profile"] = profile }
        let r = try bridge("llm.request", payload, timeout: timeout)
        var usage: LlmUsage?
        if let raw = r["usage"] as? [String: Any] {
            usage = LlmUsage(
                inputTokens: intOf(raw["inputTokens"]),
                outputTokens: intOf(raw["outputTokens"]),
                cacheReadTokens: intOf(raw["cacheReadTokens"]),
                cacheCreationTokens: intOf(raw["cacheCreationTokens"]))
        }
        let text = (r["text"] as? String) ?? (r["content"] as? String) ?? ""
        return LlmReply(text: text, model: (r["model"] as? String) ?? "",
                        finish: (r["finish"] as? String) ?? "", usage: usage)
    }

    /// Run a full agent turn — tools, multiple steps, the lot.
    ///
    /// Slower and far more capable than `llm`. Use it when the work needs the
    /// agent's tools; use `llm` when it needs a paragraph of text.
    @discardableResult
    public func agent(_ prompt: String, tools: [String]? = nil, timeout: TimeInterval = 900) throws -> [String: Any] {
        var payload: [String: Any] = ["prompt": prompt]
        if let tools, !tools.isEmpty { payload["tools"] = tools }
        return try bridge("agent.run", payload, timeout: timeout)
    }

    // -- knowledge --------------------------------------------------------
    //
    // Each *space* is an independent memory partition. Omitting `space` uses the
    // app's own private one, named after the app id — so an app that never names
    // a space can neither read nor pollute anybody else's memory.

    /// Save one memory into a knowledge space.
    public func knowledgeSave(_ text: String, space: String? = nil, source: String? = nil, tags: [String]? = nil) throws {
        var payload: [String: Any] = ["text": text]
        if let space { payload["space"] = space }
        if let source { payload["source"] = source }
        if let tags, !tags.isEmpty { payload["tags"] = tags }
        _ = try bridge("knowledge.save", payload)
    }

    /// Scoped search over one knowledge space — raw hits, no synthesis.
    public func knowledgeSearch(_ query: String, space: String? = nil, limit: Int = 10) throws -> [KnowledgeHit] {
        var payload: [String: Any] = ["query": query, "limit": limit]
        if let space { payload["space"] = space }
        let r = try bridge("knowledge.search", payload)
        guard let hits = r["hits"] as? [[String: Any]] else { return [] }
        return hits.map {
            KnowledgeHit(
                name: ($0["name"] as? String) ?? "",
                summary: ($0["summary"] as? String) ?? "",
                score: doubleOf($0["score"]))
        }
    }

    /// Scoped recall *with* LLM synthesis — one answer, not a hit list.
    ///
    /// Returns `""` when the space holds nothing relevant. That is a real answer,
    /// not an error.
    public func knowledgeRecall(_ query: String, space: String? = nil, limit: Int? = nil, hops: Int? = nil) throws -> String {
        var payload: [String: Any] = ["query": query]
        if let space { payload["space"] = space }
        if let limit { payload["limit"] = limit }
        if let hops { payload["hops"] = hops }
        let r = try bridge("knowledge.recall", payload, timeout: 300)
        return (r["answer"] as? String) ?? ""
    }

    // -- accounting & models ----------------------------------------------

    /// Report tokens for a call the app made **directly** to a provider.
    ///
    /// Only for apps holding their own API key and bypassing `llm` — it keeps the
    /// daemon's accounting whole. Fire-and-forget: a failure here must never take
    /// down the work it describes, so errors are swallowed. Pass
    /// `estimated: true` when the numbers are chars/4 guesses.
    public func usageReport(
        model: String, provider: String, inputTokens: Int, outputTokens: Int,
        latencyMs: Int = 0, estimated: Bool = false
    ) {
        _ = try? bridge("usage.report", [
            "model": model, "provider": provider,
            "inputTokens": inputTokens, "outputTokens": outputTokens,
            "latencyMs": latencyMs, "estimated": estimated,
        ])
    }

    /// The daemon's configured LLMs, and which one is active.
    public func listModels() throws -> (active: String?, models: [ModelInfo]) {
        let v = try request("GET", "/api/llm-config")
        guard let dict = v as? [String: Any] else { return (nil, []) }
        let active = dict["activeId"] as? String
        var out: [ModelInfo] = []
        for c in (dict["configs"] as? [[String: Any]]) ?? [] {
            guard let id = c["id"] as? String else { continue }
            out.append(ModelInfo(
                id: id,
                modelName: c["modelName"] as? String,
                provider: (c["provider"] as? String) ?? (c["adapt"] as? String)))
        }
        return (active, out)
    }

    /// Switch the daemon's active main model.
    ///
    /// **Global** — the agent and every other app share it. An app that wants its
    /// own model should pass `profile` to `llm` rather than moving everyone
    /// else's cheese.
    public func setActiveModel(_ id: String) throws {
        _ = try request("POST", "/api/llm-config/active", body: ["id": id])
    }

    // -- everything else --------------------------------------------------

    /// Register an MCP server with the daemon on this app's behalf.
    ///
    /// `registration` takes `transport` (`stdio` | `sse` | `http`) plus the
    /// fields that transport needs — `url`, or `command`/`args`/`env` — and
    /// optionally `name`, `description`, `use_tools`, `enabled`.
    @discardableResult
    public func registerMcp(_ registration: [String: Any]) throws -> [String: Any] {
        let r = try request("POST", appPath("/mcp/register"), body: registration)
        return (r as? [String: Any]) ?? [:]
    }

    /// Any other daemon endpoint, e.g. `core("GET", "/api/wiki/list")`.
    @discardableResult
    public func core(_ method: String, _ path: String, body: Any? = nil) throws -> Any? {
        let p = path.hasPrefix("/") ? path : "/" + path
        return try request(method, p, body: body)
    }
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// Holds a `dataTask` result across the semaphore. `@unchecked Sendable`
/// because the write (in the handler) strictly happens-before the read (after
/// `wait()`); the semaphore is the barrier.
private final class ResultBox: @unchecked Sendable {
    var data: Data?
    var response: URLResponse?
    var error: Error?
}

/// Dig the daemon's own message out of an error body, so the caller reads
/// "Config key not found" rather than a wall of JSON.
private func detail(_ data: Data?) -> String {
    guard let data, !data.isEmpty else { return "(empty response)" }
    if let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
        for k in ["error", "message", "detail"] {
            if let v = obj[k] as? String, !v.isEmpty { return v }
        }
    }
    let s = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    return s.isEmpty ? "(empty response)" : s
}

/// Percent-escape one path segment. The app id and a config key both land in a
/// URL path, and both can carry characters a raw interpolation would break.
func pathEscape(_ s: String) -> String {
    s.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? s
}

/// JSON numbers decode as `NSNumber`/`Double`; read one as `Int` without caring
/// which. Missing or wrong-typed reads as 0.
func intOf(_ v: Any?) -> Int {
    if let n = v as? NSNumber { return n.intValue }
    if let i = v as? Int { return i }
    if let d = v as? Double { return Int(d) }
    return 0
}

/// As `intOf`, for a floating-point field such as a similarity score.
func doubleOf(_ v: Any?) -> Double {
    if let n = v as? NSNumber { return n.doubleValue }
    if let d = v as? Double { return d }
    if let i = v as? Int { return Double(i) }
    return 0
}
