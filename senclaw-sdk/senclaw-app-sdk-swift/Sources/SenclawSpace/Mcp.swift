// Exposing a Swift Space App's tools to SenClaw agents over MCP.
//
// SenClaw's MCP client for an app is deliberately plain: JSON-RPC 2.0 objects
// POSTed to one URL, one request per response, plain `application/json` back.
// There is no session, no SSE stream and no long-lived connection to manage, so
// the whole server is a dictionary of handlers — which is why this needs no MCP
// library.
//
// Three methods are all a client ever sends:
//
//   - `initialize`  → who you are and what you support
//   - `tools/list`  → the tools, with their JSON Schemas
//   - `tools/call`  → run one
//
// `notifications/initialized` arrives too and is answered with an empty result;
// SenClaw sends it as a request rather than a notification and ignores the reply.
//
// Tool **names** are what agents type, so they follow the repo convention:
// `<prefix>_<verb>[_<modifier>]` in snake_case, reached by the agent as
// `mcp__<mcp.name>__<tool>`.

import Foundation

/// A JSON Schema, as the untyped dictionary the wire carries.
public typealias Schema = [String: Any]

/// A tool implementation: decoded arguments in, any JSON-able value (or a
/// content envelope) out.
public typealias ToolFn = (_ args: [String: Any]) throws -> Any

/// A registry of tools, and the JSON-RPC dispatcher over them.
public final class McpServer {
    public let name: String
    public let version: String
    private var tools: [String: (description: String, schema: Schema, fn: ToolFn)] = [:]
    private var order: [String] = []

    public static let protocolVersion = "2024-11-05"

    public init(_ name: String, version: String = "1.0.0") {
        self.name = name
        self.version = version
    }

    /// Register one tool.
    ///
    /// Write the schema: a tool with no schema is one the model has to guess the
    /// arguments of, and it guesses badly.
    public func tool(_ name: String, _ description: String, _ schema: Schema = ["type": "object", "properties": [:]], _ fn: @escaping ToolFn) {
        if tools[name] == nil { order.append(name) }
        tools[name] = (description, schema, fn)
    }

    public var toolNames: [String] { order }

    /// One JSON-RPC request in, one JSON-RPC response out.
    public func handle(_ request: [String: Any]) -> [String: Any] {
        let rpcId = request["id"] ?? NSNull()
        let method = (request["method"] as? String) ?? ""
        let params = (request["params"] as? [String: Any]) ?? [:]
        do {
            let result = try dispatch(method, params)
            return ["jsonrpc": "2.0", "id": rpcId, "result": result]
        } catch let e as RpcError {
            return ["jsonrpc": "2.0", "id": rpcId, "error": ["code": e.code, "message": e.message]]
        } catch {
            return ["jsonrpc": "2.0", "id": rpcId,
                    "error": ["code": -32603, "message": "\(error)"]]
        }
    }

    private func dispatch(_ method: String, _ params: [String: Any]) throws -> Any {
        switch method {
        case "initialize":
            return [
                "protocolVersion": Self.protocolVersion,
                "capabilities": ["tools": [:] as [String: Any]],
                "serverInfo": ["name": name, "version": version],
            ]
        case "notifications/initialized", "initialized", "ping":
            return [:] as [String: Any]
        case "tools/list":
            return ["tools": order.map { n -> [String: Any] in
                let t = tools[n]!
                return ["name": n, "description": t.description, "inputSchema": t.schema]
            }]
        case "tools/call":
            return try call((params["name"] as? String) ?? "", (params["arguments"] as? [String: Any]) ?? [:])
        default:
            throw RpcError(-32601, "method not found: \(method)")
        }
    }

    private func call(_ name: String, _ arguments: [String: Any]) throws -> [String: Any] {
        guard let tool = tools[name] else {
            throw RpcError(-32602, "unknown tool: \(name) (have: \(order.joined(separator: ", ")))")
        }
        do {
            return toContent(try tool.fn(arguments))
        } catch let e as RpcError {
            throw e
        } catch {
            // A thrown error becomes tool content the agent can read and act on,
            // not a JSON-RPC transport error — the agent needs the sentence that
            // says what to do differently, not a stack trace.
            return errorContent("\(error)")
        }
    }
}

/// A JSON-RPC error to return to the client, when the failure really is at the
/// protocol level (unknown method, unknown tool) rather than inside a tool.
public struct RpcError: Error {
    public let code: Int
    public let message: String
    public init(_ code: Int, _ message: String) {
        self.code = code
        self.message = message
    }
}

/// Wrap a tool's return value in the MCP content envelope.
///
/// A tool may return a string, a JSON-able value, or the envelope itself when it
/// wants control. Anything else is JSON-encoded — an agent reads text, so
/// returning a bare object that cannot be serialised is a silent nothing.
public func toContent(_ value: Any) -> [String: Any] {
    if let dict = value as? [String: Any], dict["content"] != nil { return dict }
    let text: String
    if let s = value as? String {
        text = s
    } else if JSONSerialization.isValidJSONObject(value),
        let data = try? JSONSerialization.data(withJSONObject: value, options: [.prettyPrinted]),
        let s = String(data: data, encoding: .utf8) {
        text = s
    } else {
        text = "\(value)"
    }
    return ["content": [["type": "text", "text": text]]]
}

/// A tool failure the agent can read and act on.
///
/// Returned as content with `isError`, not raised: a JSON-RPC error is a
/// transport failure and the agent sees a stack trace, where what it needs is
/// the sentence explaining what to do differently.
public func errorContent(_ message: String) -> [String: Any] {
    ["content": [["type": "text", "text": message]], "isError": true]
}
