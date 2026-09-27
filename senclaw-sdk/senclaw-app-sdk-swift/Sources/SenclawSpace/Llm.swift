// Serving an LLM *from* a Swift Space App, so SenClaw can route turns to it.
//
// This is the reverse of `SpaceClient`. There, an app asks the daemon for a
// completion. Here, the app *is* the model: it declares an `llm` block in its
// `senclaw-manifest.json`, the daemon registers the models it advertises into
// the same picker as every remote provider, and agent turns arrive over HTTP.
//
//     struct Mlx: LlmProvider {
//         func models() -> [ModelCard] {
//             [ModelCard("gemma-4-e2b-it-4bit", contextLength: 128_000, maxOutputTokens: 8192, vision: true)]
//         }
//         func chat(_ req: ChatRequest, _ sink: ChunkSink) throws {
//             sink.text("hello")
//         }
//     }
//
//     let routes = llmRoutes(Mlx())          // GET /v1/models + POST /v1/chat/completions
//     try Serve(Config(routes: routes, healthPath: "/health"))
//
// ## Why the app owns the wire format and not the provider
//
// The provider emits **semantic** events — visible text, reasoning, a tool call
// — and this module renders them as OpenAI `chat.completion.chunk` SSE. That
// split is the whole point: the daemon's OpenAI adapter is a real parser with
// real expectations (`delta.content`, `delta.reasoning_content`, indexed
// `delta.tool_calls` whose `name` and `arguments` *accumulate* across chunks),
// and every app that hand-rolled that JSON would get a different corner of it
// wrong. An app that conforms to `LlmProvider` cannot get it wrong at all.

import Foundation

/// Where the daemon looks for an app's model list while the app is **stopped**.
///
/// Relative to the app's own directory. A session app is stopped most of the
/// time — that is its resting state — and a model nobody can see in the picker
/// is a model nobody selects, calls, or ever starts the app for. So the list is
/// cached on disk at startup and read from there when the process is gone.
public let MODELS_CACHE_PATH = ".senclaw/llm-models.json"

// ============================================================================
// What a provider advertises
// ============================================================================

/// One model this app can serve.
public struct ModelCard: Equatable {
    /// Wire id. This is what arrives in `ChatRequest.model`, and what the user
    /// sees in the picker unless `displayName` says otherwise.
    public var id: String
    /// Human label for the picker. Defaults to `id`.
    public var displayName: String?
    /// Total context window, in tokens.
    public var contextLength: Int
    /// Ceiling on one response, in tokens.
    public var maxOutputTokens: Int
    /// **Required, never inferred.** SenClaw decides whether to send image blocks
    /// or fall back to OCR from this field, and the consequences are asymmetric:
    /// a text-only endpoint answers an image block with a hard 400 that fails the
    /// entire turn, while OCR merely degrades it. Inference from the model id
    /// cannot be trusted — a local checkpoint is named things like
    /// `mlx-community/Qwen3.5-2B-OptiQ-4bit`, which matches no vendor pattern. The
    /// app has the model's `config.json` open; it knows.
    public var vision: Bool
    /// Whether the model can be given tools. `false` makes it chat-only.
    public var tools: Bool

    public init(_ id: String, contextLength: Int, maxOutputTokens: Int, vision: Bool,
                displayName: String? = nil, tools: Bool = true) {
        self.id = id
        self.contextLength = contextLength
        self.maxOutputTokens = maxOutputTokens
        self.vision = vision
        self.displayName = displayName
        self.tools = tools
    }

    /// The on-disk / `/v1/models` shape. Snake_case keys, because that is what
    /// the daemon reads to build the picker entry.
    var wire: [String: Any] {
        var out: [String: Any] = [
            "id": id, "context_length": contextLength,
            "max_output_tokens": maxOutputTokens, "vision": vision, "tools": tools,
        ]
        if let displayName { out["display_name"] = displayName }
        return out
    }

    static func fromWire(_ d: [String: Any]) -> ModelCard? {
        guard let id = d["id"] as? String else { return nil }
        return ModelCard(
            id,
            contextLength: intOf(d["context_length"]),
            maxOutputTokens: intOf(d["max_output_tokens"]),
            vision: (d["vision"] as? Bool) ?? false,
            displayName: d["display_name"] as? String,
            // Absent means true — the historical default before the field
            // existed. A missing `tools` must not silently disable tool use.
            tools: (d["tools"] as? Bool) ?? true)
    }
}

// ============================================================================
// One turn
// ============================================================================

/// An incoming turn, in OpenAI `chat/completions` shape.
///
/// The modelled fields are the ones every provider needs. `raw` carries the
/// whole body besides, because SenClaw sends more than this struct names —
/// HF-style `tools`, `stream_options`, provider-specific extras — and a provider
/// that understands one of them should not have to fork the SDK to read it.
public struct ChatRequest {
    /// Which `ModelCard.id` this turn is for.
    public let model: String
    /// OpenAI-shaped messages, untouched. Kept as raw JSON values rather than a
    /// typed enum: `content` is a string on some turns and an array of parts on
    /// others (that is how images arrive), and a lossy re-encoding here would
    /// drop exactly the parts a vision model needs.
    public let messages: [Any]
    /// Tool definitions, or empty. OpenAI function shape.
    public let tools: [Any]
    /// Did the caller ask for SSE? `llmRoutes` handles both.
    public let stream: Bool
    /// Output ceiling for this turn, when the caller set one.
    public let maxTokens: Int?
    /// Sampling temperature, when the caller set one.
    public let temperature: Double?
    /// The complete request body.
    public let raw: [String: Any]

    /// Parse a request body, or throw a message describing what was wrong.
    public static func fromBody(_ body: [String: Any]) throws -> ChatRequest {
        let model = (body["model"] as? String) ?? ""
        if model.isEmpty { throw SenclawError("`model` is required") }
        guard let messages = body["messages"] as? [Any] else {
            throw SenclawError("`messages` must be an array")
        }
        if messages.isEmpty { throw SenclawError("`messages` must not be empty") }
        // `max_completion_tokens` is the current spelling; `max_tokens` is what
        // older clients (and SenClaw) still send. The newer one wins.
        let maxTokens: Int? = {
            if let v = body["max_completion_tokens"] { return intOf(v) }
            if let v = body["max_tokens"] { return intOf(v) }
            return nil
        }()
        return ChatRequest(
            model: model,
            messages: messages,
            tools: (body["tools"] as? [Any]) ?? [],
            stream: (body["stream"] as? Bool) ?? false,
            maxTokens: maxTokens,
            temperature: (body["temperature"] as? NSNumber)?.doubleValue,
            raw: body)
    }
}

/// One semantic event from a running generation.
public enum Chunk {
    /// Visible assistant text, already stripped of any chat-template markers.
    case text(String)
    /// Chain-of-thought, shown separately by SenClaw and echoed back on the next
    /// request as `reasoning_content`.
    case reasoning(String)
    /// A completed tool call. Emit it whole: the SDK renders the accumulating
    /// `delta.tool_calls` shape the OpenAI wire requires, so a provider never has
    /// to stream partial JSON arguments and hope they reassemble.
    case toolCall(id: String, name: String, arguments: String)
    /// Token counts for this turn. Emit at most once, at the end. SenClaw reads
    /// it into its usage tracking; omitting it costs only the statistics.
    case usage(promptTokens: Int, completionTokens: Int)
}

/// The handle a provider writes generation events to.
///
/// Sending after the client has disconnected is a no-op, so a provider does not
/// need to check. `isClosed` is there for one that would rather stop generating
/// than finish into a void.
public final class ChunkSink {
    private let onChunk: (Chunk) -> Void
    private let closed: () -> Bool

    init(onChunk: @escaping (Chunk) -> Void, closed: @escaping () -> Bool = { false }) {
        self.onChunk = onChunk
        self.closed = closed
    }

    public func send(_ chunk: Chunk) { onChunk(chunk) }
    /// Convenience for the common case.
    public func text(_ s: String) { onChunk(.text(s)) }
    /// Has the receiving end gone away?
    public var isClosed: Bool { closed() }
}

/// What an app conforms to become a model.
public protocol LlmProvider {
    /// Every model this app can serve, right now.
    func models() -> [ModelCard]

    /// Run one turn, writing events to `sink` as they happen.
    ///
    /// Throwing after events have already been sent ends the stream early; the
    /// client keeps what it received. Load weights here, lazily — **not** at
    /// startup. The daemon health-gates a newly spawned app on a 30-second budget
    /// with a 5-second probe, so an app that loads gigabytes before it binds its
    /// port is reported as failing to start.
    func chat(_ req: ChatRequest, _ sink: ChunkSink) throws
}

// ============================================================================
// The routes
// ============================================================================

/// `GET {prefix}/models` + `POST {prefix}/chat/completions` for a provider,
/// ready to merge into `Serve`'s routes. `prefix` defaults to `/v1` — mount it
/// where the manifest's `llm.path` says.
public func llmRoutes(_ provider: LlmProvider, prefix: String = "/v1") -> [RouteKey: Handler] {
    let base = "/" + prefix.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
    return [
        RouteKey("GET", "\(base)/models"): { _ in
            let data = provider.models().map { m -> [String: Any] in
                var entry = m.wire
                entry["object"] = "model"
                entry["owned_by"] = "senclaw-space-app"
                return entry
            }
            return Response(json: ["object": "list", "data": data])
        },
        RouteKey("POST", "\(base)/chat/completions"): { req in
            chatCompletions(provider, req)
        },
    ]
}

private func chatCompletions(_ provider: LlmProvider, _ req: Request) -> Response {
    let body = ((try? req.json()) as? [String: Any]) ?? [:]
    let chat: ChatRequest
    do {
        chat = try ChatRequest.fromBody(body)
    } catch {
        return errorBody(400, "\(error)")
    }
    if !provider.models().contains(where: { $0.id == chat.model }) {
        return errorBody(404, "unknown model `\(chat.model)`")
    }

    if !chat.stream {
        var text = ""
        var reasoning = ""
        var calls: [[String: Any]] = []
        var usage: [String: Any]?
        let sink = ChunkSink(onChunk: { c in accumulate(c, &text, &reasoning, &calls, &usage) })
        do {
            try provider.chat(chat, sink)
        } catch {
            // A client that gets 200 with half an answer and no explanation
            // cannot tell a short reply from a crash.
            return errorBody(500, "\(error)")
        }
        return Response(json: nonStreamBody(chat.model, text, reasoning, calls, usage))
    }

    // Streaming: the status line goes out with the first byte, so a failure here
    // cannot become a 5xx — `runStream` emits it as an error chunk instead, and
    // always terminates with `[DONE]` so a crashed generation does not read as a
    // hung one. `isClosed` lets a provider abandon a turn nobody is reading.
    return Response.eventStream { writer in
        runStream(provider, chat, emit: { writer.data($0) }, isClosed: { writer.isClosed })
    }
}

/// Run one turn and hand each rendered SSE payload to `emit`, in order, ending
/// with `[DONE]`. Shared by the live router and the tests — the router's `emit`
/// writes to the socket, a test's collects into an array — so the exact bytes a
/// client would see are the exact bytes a test inspects.
func runStream(_ provider: LlmProvider, _ chat: ChatRequest, emit: @escaping (String) -> Void, isClosed: @escaping () -> Bool = { false }) {
    let id = completionId()
    var index = 0
    let sink = ChunkSink(onChunk: { c in
        if let payload = renderChunk(id, chat.model, c, &index) { emit(payload) }
    }, closed: isClosed)
    do {
        try provider.chat(chat, sink)
    } catch {
        emit(jsonString([
            "id": id, "object": "chat.completion.chunk", "model": chat.model,
            "choices": [["index": 0, "delta": [:] as [String: Any], "finish_reason": "error"]],
            "error": ["message": "\(error)", "type": "server_error"],
        ]))
    }
    emit("[DONE]")
}

// ============================================================================
// Rendering
// ============================================================================

private func accumulate(_ chunk: Chunk, _ text: inout String, _ reasoning: inout String,
                        _ calls: inout [[String: Any]], _ usage: inout [String: Any]?) {
    switch chunk {
    case .text(let s): text += s
    case .reasoning(let s): reasoning += s
    case .toolCall(let id, let name, let arguments):
        calls.append([
            "id": id, "type": "function",
            "function": ["name": name, "arguments": arguments],
        ])
    case .usage(let prompt, let completion):
        usage = [
            "prompt_tokens": prompt, "completion_tokens": completion,
            "total_tokens": prompt + completion,
        ]
    }
}

/// Render one event as a `chat.completion.chunk` `data:` payload, or `nil` for
/// an empty text/reasoning chunk that carries nothing.
///
/// The tool-call shape is the fiddly part and the reason this is not left to
/// apps: the consumer accumulates `function.name` and `function.arguments` by
/// **concatenation** across chunks at a given `index`, so a whole call must go
/// out as a single delta at a fresh index. Sending the name twice, or reusing an
/// index, silently produces `get_weatherget_weather`.
func renderChunk(_ id: String, _ model: String, _ chunk: Chunk, _ index: inout Int) -> String? {
    let delta: [String: Any]
    switch chunk {
    case .text(let s):
        if s.isEmpty { return nil }
        delta = ["content": s]
    case .reasoning(let s):
        if s.isEmpty { return nil }
        delta = ["reasoning_content": s]
    case .toolCall(let callId, let name, let arguments):
        let i = index
        index += 1
        delta = ["tool_calls": [[
            "index": i, "id": callId, "type": "function",
            "function": ["name": name, "arguments": arguments],
        ]]]
    case .usage(let prompt, let completion):
        // Usage rides its own chunk with an empty `choices` array — the shape
        // `stream_options.include_usage` produces, and the one the consumer
        // looks for it in.
        return jsonString([
            "id": id, "object": "chat.completion.chunk", "model": model,
            "choices": [] as [Any],
            "usage": ["prompt_tokens": prompt, "completion_tokens": completion,
                      "total_tokens": prompt + completion],
        ])
    }
    return jsonString([
        "id": id, "object": "chat.completion.chunk", "model": model,
        "choices": [["index": 0, "delta": delta, "finish_reason": NSNull()]],
    ])
}

func nonStreamBody(_ model: String, _ text: String, _ reasoning: String,
                   _ calls: [[String: Any]], _ usage: [String: Any]?) -> [String: Any] {
    var message: [String: Any] = ["role": "assistant", "content": text]
    if !reasoning.isEmpty { message["reasoning_content"] = reasoning }
    if !calls.isEmpty { message["tool_calls"] = calls }
    var out: [String: Any] = [
        "id": completionId(), "object": "chat.completion", "model": model,
        "choices": [[
            "index": 0, "message": message,
            "finish_reason": calls.isEmpty ? "stop" : "tool_calls",
        ]],
    ]
    if let usage { out["usage"] = usage }
    return out
}

/// `chatcmpl-<hex>`. Uniqueness only has to hold within one client's stream, so
/// process id plus a monotonic counter is enough and pulls in no dependency.
private let completionCounter = Counter()
func completionId() -> String {
    String(format: "chatcmpl-%x%x", getpid(), completionCounter.next())
}

private final class Counter: @unchecked Sendable {
    private var n: UInt64 = 0
    private let lock = NSLock()
    func next() -> UInt64 {
        lock.lock(); defer { lock.unlock() }
        let v = n; n += 1; return v
    }
}

private func errorBody(_ status: Int, _ message: String) -> Response {
    Response(json: ["error": ["message": message, "type": "invalid_request_error"]], status: status)
}

/// Compact JSON string for one SSE payload.
func jsonString(_ obj: Any) -> String {
    guard let data = try? JSONSerialization.data(withJSONObject: obj), let s = String(data: data, encoding: .utf8) else {
        return "{}"
    }
    return s
}

// ============================================================================
// Model cache
// ============================================================================

/// Write the model list to `<appDir>/.senclaw/llm-models.json`, for the daemon
/// to read while this app is stopped. Call it once at startup, after the models
/// are known.
///
/// An empty list is refused rather than written. The daemon treats a missing
/// cache as "not known yet" and a present one as authoritative, so clobbering a
/// good list with an empty one during a failed startup would remove the app's
/// models from the picker until someone noticed.
public func publishModels(_ appDir: String, _ models: [ModelCard]) throws {
    if models.isEmpty {
        throw SenclawError("refusing to publish an empty model list")
    }
    let path = URL(fileURLWithPath: appDir).appendingPathComponent(MODELS_CACHE_PATH)
    try FileManager.default.createDirectory(
        at: path.deletingLastPathComponent(), withIntermediateDirectories: true)
    let body = try JSONSerialization.data(
        withJSONObject: ["models": models.map(\.wire)], options: [.prettyPrinted, .sortedKeys])
    // Write-then-rename: a daemon reading this file concurrently sees either the
    // old list or the new one, never a truncated one.
    let tmp = path.appendingPathExtension("tmp")
    try body.write(to: tmp)
    if FileManager.default.fileExists(atPath: path.path) {
        _ = try? FileManager.default.removeItem(at: path)
    }
    try FileManager.default.moveItem(at: tmp, to: path)
}
