// Serving a Swift Space App: health, static UI, REST and MCP in one process.
//
// The daemon expects one HTTP server per app, on the port it hands out in
// `PORT`, answering:
//
//   - `runtime.healthPath` — anything 2xx. The daemon waits on this before it
//     considers the app started (30s budget), and the supervisor polls it.
//   - `mcp.path` — the app's MCP endpoint, JSON-RPC over HTTP POST.
//   - everything else — the app's own REST API and its UI, which the daemon
//     reverse-proxies at `/api/space/apps/<id>/proxy/…`.
//
// There is no HTTP server in the Swift standard library, so this is a small
// blocking one on POSIX sockets — thread per connection, `Connection: close`,
// `Content-Length` bodies, plus SSE for the streaming `llm` router. Foundation
// only; nothing to install.
//
// The one thing worth reading before writing an app: **handle SIGTERM**. A
// session app is stopped when it goes idle, and the daemon signals the process
// group with SIGTERM and SIGKILLs it two seconds later. Two seconds is plenty
// to flush, and nothing if you ignore the signal — `Serve` installs a handler
// that runs `onShutdown` and closes the listener.

import Foundation

#if canImport(Glibc)
import Glibc
#else
import Darwin
#endif

// ---------------------------------------------------------------------------
// request / response
// ---------------------------------------------------------------------------

/// One incoming HTTP request, reduced to what an app handler needs.
public struct Request {
    public let method: String
    public let path: String
    public let query: [String: [String]]
    public let headers: [String: String]  // keys lower-cased
    public let body: Data

    /// The request body as JSON, or `nil` when there is no body.
    public func json() throws -> Any? {
        if body.isEmpty { return nil }
        return try JSONSerialization.jsonObject(with: body, options: [.fragmentsAllowed])
    }

    /// One query parameter, or `nil`.
    public func param(_ name: String) -> String? { query[name]?.first }

    /// One header, looked up case-insensitively.
    public func header(_ name: String) -> String? { headers[name.lowercased()] }
}

/// What a handler returns. Return a `Response` for control over status or
/// content type; the built-in `json`/`text` initialisers cover the common cases.
public struct Response {
    public var status: Int
    public var contentType: String
    public var headers: [String: String]
    var body: Body

    enum Body {
        case data(Data)
        /// A streaming SSE body — the closure writes events as they are produced
        /// and returns when done. Used by the `llm` router.
        case stream((SSEWriter) -> Void)
    }

    public init(json: Any, status: Int = 200) {
        self.status = status
        self.contentType = "application/json"
        self.headers = [:]
        let data = (try? JSONSerialization.data(withJSONObject: json)) ?? Data("null".utf8)
        self.body = .data(data)
    }

    public init(text: String, status: Int = 200, contentType: String = "text/plain; charset=utf-8") {
        self.status = status
        self.contentType = contentType
        self.headers = [:]
        self.body = .data(Data(text.utf8))
    }

    public init(data: Data, contentType: String, status: Int = 200) {
        self.status = status
        self.contentType = contentType
        self.headers = [:]
        self.body = .data(data)
    }

    /// A `text/event-stream` response whose body is written incrementally.
    public static func eventStream(_ writer: @escaping (SSEWriter) -> Void) -> Response {
        var r = Response(data: Data(), contentType: "text/event-stream")
        r.body = .stream(writer)
        return r
    }
}

/// The handle a streaming response writes SSE events to.
public final class SSEWriter {
    private let fd: Int32
    private var broken = false

    init(_ fd: Int32) { self.fd = fd }

    /// Send one `data:` event. `[DONE]` is a payload like any other.
    public func data(_ payload: String) { writeRaw("data: \(payload)\n\n") }

    /// The client is gone once a write fails (EPIPE). A provider generating a
    /// long answer can poll this and abandon a turn nobody is reading.
    public var isClosed: Bool { broken }

    private func writeRaw(_ s: String) {
        if !sendAll(fd, Data(s.utf8)) { broken = true }
    }
}

/// A `(method, path)` route key.
public struct RouteKey: Hashable {
    public let method: String
    public let path: String
    public init(_ method: String, _ path: String) {
        self.method = method.uppercased()
        self.path = path
    }
}

/// A request handler.
public typealias Handler = (Request) -> Response

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// Describes the app's HTTP surface.
public struct Config {
    /// `(method, path)` → handler. A path ending in `/*` matches by prefix and
    /// the handler gets the full path.
    public var routes: [RouteKey: Handler]
    /// `runtime.healthPath`. Registering your own route at the same path takes
    /// precedence — an app that reports its real state must not be overwritten
    /// with `{"ok":true}`. Defaults to `/health`.
    public var healthPath: String
    /// The app's built web UI, served with an `index.html` fallback for unknown
    /// paths so a client-side router works.
    public var staticDir: String?
    /// The manifest's `mcp.path`.
    public var mcpPath: String?
    /// Usually an `McpServer`.
    public var mcp: McpServer?
    /// Runs on SIGTERM/SIGINT before the listener closes. Budget about two
    /// seconds: flush and close, do not start new work.
    public var onShutdown: (() -> Void)?
    /// Used when `PORT` is unset — running the app by hand.
    public var defaultPort: Int
    /// Refuse any request that does not carry this app's access token, closing
    /// the app's own API to everything except the daemon. Off by default — see
    /// `appTokenAuthorized`.
    public var requireAppToken: Bool
    /// Paths exempt from `requireAppToken` (exact, or a trailing `/*` prefix).
    /// The health path is always exempt.
    public var authSkipPaths: [String]
    public var log: (String) -> Void

    public init(
        routes: [RouteKey: Handler] = [:], healthPath: String = "/health",
        staticDir: String? = nil, mcpPath: String? = nil, mcp: McpServer? = nil,
        onShutdown: (() -> Void)? = nil, defaultPort: Int = 0,
        requireAppToken: Bool = false, authSkipPaths: [String] = [],
        log: @escaping (String) -> Void = { print($0) }
    ) {
        self.routes = routes
        self.healthPath = healthPath
        self.staticDir = staticDir
        self.mcpPath = mcpPath
        self.mcp = mcp
        self.onShutdown = onShutdown
        self.defaultPort = defaultPort
        self.requireAppToken = requireAppToken
        self.authSkipPaths = authSkipPaths
        self.log = log
    }
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

/// Run the app's HTTP server until the daemon stops it. Blocks.
public func Serve(_ config: Config) throws {
    // A write to a socket the client already closed would otherwise kill the
    // process with SIGPIPE; we handle the failed write instead.
    signal(SIGPIPE, SIG_IGN)

    let host = bindHost()
    let listenPort = try appPort(config.defaultPort)
    let staticRoot = config.staticDir.map { URL(fileURLWithPath: $0).standardizedFileURL.path }
    let guardToken = config.requireAppToken ? appTokenFromEnv() : ""
    let skip = [config.healthPath] + config.authSkipPaths

    // Precompute exact and prefix routes.
    var exact: [RouteKey: Handler] = [:]
    var prefixes: [(method: String, prefix: String, handler: Handler)] = []
    for (key, handler) in config.routes {
        if key.path.hasSuffix("/*") {
            prefixes.append((key.method, String(key.path.dropLast()), handler))
        } else {
            exact[key] = handler
        }
    }

    let listenFd = try openListener(host: host, port: listenPort)
    let running = Flag(true)

    // SIGTERM is what the daemon sends when it stops an idle session app, and
    // what it sends every app on its own shutdown. Two seconds later it is
    // SIGKILL, so anything unflushed at that point is lost.
    let sources = [SIGTERM, SIGINT].map { sig -> DispatchSourceSignal in
        signal(sig, SIG_IGN)
        let src = DispatchSource.makeSignalSource(signal: sig, queue: .global())
        src.setEventHandler {
            guard running.get() else { return }
            running.set(false)
            config.log("[senclaw] signal \(sig) — shutting down")
            config.onShutdown?()
        }
        src.resume()
        return src
    }
    _ = sources  // keep the sources alive for the lifetime of Serve

    config.log("[senclaw] listening on http://\(host):\(listenPort)")

    // Poll before accept so shutdown is noticed within the timeout rather than
    // blocking forever in accept() on a socket nobody will connect to again.
    while running.get() {
        var pfd = pollfd(fd: listenFd, events: Int16(POLLIN), revents: 0)
        let ready = poll(&pfd, 1, 300)
        if ready <= 0 { continue }
        let clientFd = accept(listenFd, nil, nil)
        if clientFd < 0 { continue }
        Thread.detachNewThread {
            handleConnection(
                clientFd, exact: exact, prefixes: prefixes, healthPath: config.healthPath,
                staticRoot: staticRoot, mcpPath: config.mcpPath, mcp: config.mcp,
                guardToken: guardToken, skip: skip)
        }
    }

    close(listenFd)
}

private func openListener(host: String, port: Int) throws -> Int32 {
    let fd = socket(AF_INET, sockStreamType, 0)
    if fd < 0 { throw SenclawError("socket() failed: \(errnoString())") }

    var yes: Int32 = 1
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))

    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port = in_port_t(port).bigEndian
    if inet_pton(AF_INET, host, &addr.sin_addr) != 1 {
        // Not a numeric address — fall back to loopback rather than binding the
        // wildcard interface, which a Space App must never do by accident.
        _ = inet_pton(AF_INET, "127.0.0.1", &addr.sin_addr)
    }

    let bindResult = withUnsafePointer(to: &addr) { ptr in
        ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    if bindResult < 0 {
        close(fd)
        throw SenclawError("bind(\(host):\(port)) failed: \(errnoString())")
    }
    if listen(fd, 128) < 0 {
        close(fd)
        throw SenclawError("listen() failed: \(errnoString())")
    }
    return fd
}

private func handleConnection(
    _ fd: Int32, exact: [RouteKey: Handler],
    prefixes: [(method: String, prefix: String, handler: Handler)],
    healthPath: String, staticRoot: String?, mcpPath: String?, mcp: McpServer?,
    guardToken: String, skip: [String]
) {
    defer { close(fd) }
    guard let req = readRequest(fd) else { return }

    // Only the daemon gets to talk to this port, when the guard is on. Checked
    // before routing so an unauthorised caller never reaches a handler.
    if !guardToken.isEmpty {
        let presented = req.header(HEADER_APP_TOKEN) ?? req.param("app_token")
        if !appTokenAuthorized(path: req.path, presented: presented, token: guardToken, skip: skip) {
            write(fd, req.method, Response(json: [
                "error": "this app only answers requests from the SenClaw daemon",
                "code": "app_token_required",
            ], status: 401))
            return
        }
    }

    // MCP endpoint — plain JSON-RPC over POST.
    if let mcpPath, let mcp, req.path == mcpPath {
        guard let obj = (try? req.json()) as? [String: Any] else {
            write(fd, req.method, Response(json: [
                "jsonrpc": "2.0", "id": NSNull(),
                "error": ["code": -32700, "message": "parse error"],
            ], status: 400))
            return
        }
        write(fd, req.method, Response(json: mcp.handle(obj)))
        return
    }

    if let handler = exact[RouteKey(req.method, req.path)] {
        write(fd, req.method, handler(req))
        return
    }
    for p in prefixes where p.method == req.method && req.path.hasPrefix(p.prefix) {
        write(fd, req.method, p.handler(req))
        return
    }

    // Built-in health, after the routes so an app that reports its real state at
    // the same path answers with that.
    if req.path == healthPath, req.method == "GET" || req.method == "HEAD" {
        write(fd, req.method, Response(json: ["ok": true]))
        return
    }

    if let staticRoot, req.method == "GET" || req.method == "HEAD",
        let served = serveStatic(req.path, staticRoot) {
        write(fd, req.method, served)
        return
    }

    write(fd, req.method, Response(json: ["error": "not found", "path": req.path], status: 404))
}

/// A single-page app: unknown paths are routes, not missing files, so an unknown
/// path falls back to `index.html`. The realpath check is what stops
/// `../../etc/passwd` from being served — the one a hand-rolled static handler
/// usually forgets.
private func serveStatic(_ path: String, _ root: String) -> Response? {
    var rel = String(path.drop(while: { $0 == "/" }))
    if rel.isEmpty { rel = "index.html" }
    let target = URL(fileURLWithPath: root).appendingPathComponent(rel).standardizedFileURL
    if target.path != root && !target.path.hasPrefix(root + "/") {
        return Response(json: ["error": "forbidden"], status: 403)
    }
    var file = target
    var isDir: ObjCBool = false
    if FileManager.default.fileExists(atPath: file.path, isDirectory: &isDir), isDir.boolValue {
        file = file.appendingPathComponent("index.html")
    }
    if !FileManager.default.fileExists(atPath: file.path) {
        let index = URL(fileURLWithPath: root).appendingPathComponent("index.html")
        guard FileManager.default.fileExists(atPath: index.path) else { return nil }
        file = index
    }
    guard let data = try? Data(contentsOf: file) else { return nil }
    return Response(data: data, contentType: contentType(for: file.pathExtension))
}

// ---------------------------------------------------------------------------
// HTTP/1.1 read + write
// ---------------------------------------------------------------------------

private let maxBodyBytes = 32 << 20

private func readRequest(_ fd: Int32) -> Request? {
    var buffer = Data()
    let separator = Data("\r\n\r\n".utf8)
    var headerEnd: Int?
    // Read until the blank line that ends the headers.
    while headerEnd == nil {
        if let r = buffer.range(of: separator) {
            headerEnd = r.lowerBound
            break
        }
        guard let chunk = recvChunk(fd), !chunk.isEmpty else { return nil }
        buffer.append(chunk)
        if buffer.count > 1 << 20 { return nil }  // header flood guard
    }
    guard let end = headerEnd else { return nil }
    let headerData = buffer.subdata(in: buffer.startIndex..<end)
    guard let headerText = String(data: headerData, encoding: .utf8) else { return nil }

    var lines = headerText.components(separatedBy: "\r\n")
    guard let requestLine = lines.first else { return nil }
    lines.removeFirst()
    let parts = requestLine.split(separator: " ")
    guard parts.count >= 2 else { return nil }
    let method = String(parts[0]).uppercased()
    let rawTarget = String(parts[1])

    var headers: [String: String] = [:]
    for line in lines where !line.isEmpty {
        guard let colon = line.firstIndex(of: ":") else { continue }
        let name = String(line[line.startIndex..<colon]).trimmingCharacters(in: .whitespaces).lowercased()
        let value = String(line[line.index(after: colon)...]).trimmingCharacters(in: .whitespaces)
        headers[name] = value
    }

    // Body: exactly Content-Length bytes, some of which may already be buffered.
    var body = Data()
    if let lenStr = headers["content-length"], let length = Int(lenStr), length > 0 {
        if length > maxBodyBytes { return nil }
        let bodyStart = end + separator.count
        if bodyStart < buffer.count {
            body = buffer.subdata(in: bodyStart..<buffer.count)
        }
        while body.count < length {
            guard let chunk = recvChunk(fd), !chunk.isEmpty else { break }
            body.append(chunk)
        }
        if body.count > length { body = body.subdata(in: 0..<length) }
    }

    let (path, query) = splitTarget(rawTarget)
    return Request(method: method, path: path, query: query, headers: headers, body: body)
}

private func splitTarget(_ target: String) -> (String, [String: [String]]) {
    guard let q = target.firstIndex(of: "?") else {
        return (target.removingPercentEncoding ?? target, [:])
    }
    let path = String(target[target.startIndex..<q])
    var query: [String: [String]] = [:]
    for pair in target[target.index(after: q)...].split(separator: "&") {
        let kv = pair.split(separator: "=", maxSplits: 1)
        let key = String(kv[0]).removingPercentEncoding ?? String(kv[0])
        let value = kv.count > 1 ? (String(kv[1]).removingPercentEncoding ?? String(kv[1])) : ""
        query[key, default: []].append(value)
    }
    return (path.removingPercentEncoding ?? path, query)
}

private func recvChunk(_ fd: Int32) -> Data? {
    var buf = [UInt8](repeating: 0, count: 65536)
    let n = recv(fd, &buf, buf.count, 0)
    if n <= 0 { return nil }
    return Data(buf[0..<n])
}

private func write(_ fd: Int32, _ method: String, _ resp: Response) {
    switch resp.body {
    case .data(let body):
        var head = "HTTP/1.1 \(resp.status) \(reasonPhrase(resp.status))\r\n"
        head += "Content-Type: \(resp.contentType)\r\n"
        head += "Content-Length: \(body.count)\r\n"
        head += "Connection: close\r\n"
        for (k, v) in resp.headers { head += "\(k): \(v)\r\n" }
        head += "\r\n"
        _ = sendAll(fd, Data(head.utf8))
        if method != "HEAD" { _ = sendAll(fd, body) }
    case .stream(let writer):
        var head = "HTTP/1.1 \(resp.status) \(reasonPhrase(resp.status))\r\n"
        head += "Content-Type: \(resp.contentType)\r\n"
        head += "Cache-Control: no-cache\r\n"
        head += "Connection: close\r\n"
        for (k, v) in resp.headers { head += "\(k): \(v)\r\n" }
        head += "\r\n"
        _ = sendAll(fd, Data(head.utf8))
        if method != "HEAD" { writer(SSEWriter(fd)) }
    }
}

/// Write every byte, looping over short writes. Returns false on a broken pipe.
@discardableResult
func sendAll(_ fd: Int32, _ data: Data) -> Bool {
    data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Bool in
        guard var ptr = raw.baseAddress else { return true }
        var remaining = raw.count
        while remaining > 0 {
            let n = send(fd, ptr, remaining, sendFlags)
            if n <= 0 { return false }
            ptr = ptr.advanced(by: n)
            remaining -= n
        }
        return true
    }
}

// ---------------------------------------------------------------------------
// platform shims + small helpers
// ---------------------------------------------------------------------------

#if canImport(Glibc)
private let sockStreamType = Int32(SOCK_STREAM.rawValue)
private let sendFlags = Int32(MSG_NOSIGNAL)
#else
private let sockStreamType = SOCK_STREAM
private let sendFlags: Int32 = 0  // SIGPIPE is ignored process-wide instead
#endif

private func errnoString() -> String { String(cString: strerror(errno)) }

private func reasonPhrase(_ status: Int) -> String {
    switch status {
    case 200: return "OK"
    case 400: return "Bad Request"
    case 401: return "Unauthorized"
    case 403: return "Forbidden"
    case 404: return "Not Found"
    case 405: return "Method Not Allowed"
    case 500: return "Internal Server Error"
    default: return "Status"
    }
}

private func contentType(for ext: String) -> String {
    switch ext.lowercased() {
    case "html", "htm": return "text/html; charset=utf-8"
    case "js", "mjs": return "text/javascript; charset=utf-8"
    case "css": return "text/css; charset=utf-8"
    case "json": return "application/json"
    case "svg": return "image/svg+xml"
    case "png": return "image/png"
    case "jpg", "jpeg": return "image/jpeg"
    case "gif": return "image/gif"
    case "webp": return "image/webp"
    case "ico": return "image/x-icon"
    case "woff2": return "font/woff2"
    case "woff": return "font/woff"
    case "wasm": return "application/wasm"
    case "txt": return "text/plain; charset=utf-8"
    default: return "application/octet-stream"
    }
}

/// A lock-guarded flag shared between the accept loop and the signal handler.
final class Flag: @unchecked Sendable {
    private var value: Bool
    private let lock = NSLock()
    init(_ value: Bool) { self.value = value }
    func get() -> Bool { lock.lock(); defer { lock.unlock() }; return value }
    func set(_ v: Bool) { lock.lock(); value = v; lock.unlock() }
}
