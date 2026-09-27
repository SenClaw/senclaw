// Autonomous work dispatch — the app side of the contract.
//
// The daemon's `MCPDispatcher` engine can drive any app that exposes four
// endpoints. Implement `DispatchProvider` over your own store, hand it to
// `dispatchRoutes`, merge the result into `Serve`'s routes, and the engine will
// claim work from you, keep leases alive, recover items whose worker died, and
// report terminal outcomes back.
//
//     let routes = dispatchRoutes(provider).merging(myRoutes) { _, b in b }
//     try Serve(Config(routes: routes, healthPath: "/api/status"))
//
// The wire shape is the Rust SDK's, field for field, because the same engine
// parses both: snake_case JSON, `Outcome` tagged by `status`, `Workspace`
// tagged by `kind`, `McpServerSpec` tagged by `transport`. Note `depends_on`
// and `timeout_secs` — a camelCase spelling is silently dropped by the engine's
// serde, which surfaces as a dependency that never held rather than as an error.

import Foundation

/// How many workers the dispatcher can spawn right now.
public struct Capacity {
    /// Max items to claim across this source this tick.
    public var total: Int
    /// Max concurrent items per assignee (worker lane). 0 = unlimited.
    public var perAssignee: Int

    public init(total: Int = 0, perAssignee: Int = 0) {
        self.total = total
        self.perAssignee = perAssignee
    }

    static func fromJSON(_ v: Any?) -> Capacity {
        guard let d = v as? [String: Any] else { return Capacity() }
        return Capacity(total: intOf(d["total"]), perAssignee: intOf(d["per_assignee"]))
    }
}

/// Where a worker runs. Build one with the `workspace*` helpers so the tagged
/// shape is never hand-written.
public enum Workspace {
    /// Fresh temp dir, discarded when the worker finishes.
    case scratch
    /// A persistent absolute path.
    case dir(String)
    /// A git worktree, for coding tasks.
    case worktree(repo: String, branch: String?)

    var json: [String: Any] {
        switch self {
        case .scratch: return ["kind": "scratch"]
        case .dir(let p): return ["kind": "dir", "path": p]
        case .worktree(let repo, let branch):
            var out: [String: Any] = ["kind": "worktree", "repo": repo]
            if let branch { out["branch"] = branch }
            return out
        }
    }
}

/// An MCP server a worker needs.
///
/// Prefer `.stdio` — an `.http` spec has to be bridged to stdio by the engine at
/// launch, which is one more process and one more failure mode.
public enum McpServerSpec {
    case stdio(name: String, command: String, args: [String], env: [String: String])
    case http(name: String, url: String)

    var json: [String: Any] {
        switch self {
        case .stdio(let name, let command, let args, let env):
            return ["transport": "stdio", "name": name, "command": command, "args": args, "env": env]
        case .http(let name, let url):
            return ["transport": "http", "name": name, "url": url]
        }
    }
}

/// A single dispatchable unit of work.
public struct WorkItem {
    /// Source-scoped id, opaque to the engine.
    public var id: String
    /// The task to run — becomes the agent's user prompt.
    public var prompt: String
    /// Worker/persona to route to. `nil` = the source's default persona.
    public var assignee: String?
    /// Extra system-prompt block appended to the persona's own.
    public var guidance: String?
    /// MCP servers the worker gets, usually including this app's own tools.
    public var mcp: [McpServerSpec]
    /// Where the worker runs. Defaults to scratch.
    public var workspace: Workspace
    /// Ids this item depends on. Already satisfied by the time you return it.
    public var dependsOn: [String]
    /// Higher runs first.
    public var priority: Int
    /// Per-item run timeout, in seconds.
    public var timeoutSecs: Int?

    public init(
        id: String, prompt: String, assignee: String? = nil, guidance: String? = nil,
        mcp: [McpServerSpec] = [], workspace: Workspace = .scratch,
        dependsOn: [String] = [], priority: Int = 0, timeoutSecs: Int? = nil
    ) {
        self.id = id
        self.prompt = prompt
        self.assignee = assignee
        self.guidance = guidance
        self.mcp = mcp
        self.workspace = workspace
        self.dependsOn = dependsOn
        self.priority = priority
        self.timeoutSecs = timeoutSecs
    }

    // snake_case on the wire — the engine's serde reads exactly these keys.
    var json: [String: Any] {
        var out: [String: Any] = [
            "id": id, "prompt": prompt, "mcp": mcp.map(\.json),
            "workspace": workspace.json, "depends_on": dependsOn, "priority": priority,
        ]
        out["assignee"] = assignee as Any
        out["guidance"] = guidance as Any
        out["timeout_secs"] = timeoutSecs as Any
        return out
    }
}

/// The terminal result of a worker run. Build one with the `outcome*` helpers.
public typealias Outcome = [String: Any]

/// Terminal outcome: the work is done.
public func outcomeCompleted(summary: String = "", metadata: Any? = nil) -> Outcome {
    ["status": "completed", "summary": summary, "metadata": metadata as Any]
}
/// Terminal outcome: the worker cannot proceed and a human must look.
public func outcomeBlocked(_ reason: String) -> Outcome { ["status": "blocked", "reason": reason] }
/// Terminal outcome: the work was attempted and did not succeed.
public func outcomeFailed(_ error: String) -> Outcome { ["status": "failed", "error": error] }
/// Terminal outcome: the worker ran past its timeout.
public func outcomeTimedOut() -> Outcome { ["status": "timed_out"] }

/// Implement over your own store. Only `claimReady` and `finalize` are mandatory
/// in practice; the other two have sane no-op defaults for a source with no
/// lease model.
public protocol DispatchProvider {
    /// Atomically claim up to `capacity` ready items.
    ///
    /// **Atomically** matters: the engine may poll again before these items
    /// finish, and an item handed out twice is run twice.
    func claimReady(_ capacity: Capacity) throws -> [WorkItem]
    /// Extend the lease on an in-flight item. No-op if you have no leases.
    func heartbeat(_ itemId: String) throws
    /// Return dead-worker/expired-lease items to ready; return their ids.
    func reclaim() throws -> [String]
    /// Record a terminal outcome. Map it to your own states.
    func finalize(_ itemId: String, _ outcome: Outcome) throws
}

public extension DispatchProvider {
    func heartbeat(_ itemId: String) throws {}
    func reclaim() throws -> [String] { [] }
}

/// Build the four routes — `POST {prefix}/poll`, `/heartbeat`, `/reclaim`,
/// `/finalize` — ready to merge into `Serve`'s routes. Same paths and payloads
/// the Rust `dispatch_router` serves.
///
/// Errors become `500 {error}` rather than propagating: the engine reads that
/// field and backs off, whereas an exception reaching the HTTP layer arrives as
/// a connection reset it cannot distinguish from a crash.
public func dispatchRoutes(_ provider: DispatchProvider, prefix: String = "/api/dispatch") -> [RouteKey: Handler] {
    let base = "/" + prefix.trimmingCharacters(in: CharacterSet(charactersIn: "/"))

    func body(_ req: Request) -> [String: Any] {
        (try? req.json() as? [String: Any]) ?? [:]
    }
    func err(_ e: Error) -> Response {
        Response(json: ["error": "\(e)"], status: 500)
    }

    return [
        RouteKey("POST", "\(base)/poll"): { req in
            do {
                let items = try provider.claimReady(Capacity.fromJSON(body(req)["capacity"]))
                return Response(json: items.map(\.json))
            } catch { return err(error) }
        },
        RouteKey("POST", "\(base)/heartbeat"): { req in
            do {
                try provider.heartbeat((body(req)["item_id"] as? String) ?? "")
                return Response(json: ["ok": true])
            } catch { return err(error) }
        },
        RouteKey("POST", "\(base)/reclaim"): { _ in
            do { return Response(json: try provider.reclaim()) } catch { return err(error) }
        },
        RouteKey("POST", "\(base)/finalize"): { req in
            do {
                let b = body(req)
                let outcome = (b["outcome"] as? [String: Any]) ?? outcomeFailed("no outcome sent")
                try provider.finalize((b["item_id"] as? String) ?? "", outcome)
                return Response(json: ["ok": true])
            } catch { return err(error) }
        },
    ]
}
