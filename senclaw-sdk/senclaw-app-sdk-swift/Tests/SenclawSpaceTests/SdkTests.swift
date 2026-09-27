import Foundation
import XCTest

@testable import SenclawSpace

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

final class ManifestTests: XCTestCase {
    func testAGoodManifestHasNoProblems() {
        let m = manifest(
            id: "demo", name: "Demo", description: "x",
            runtime: runtimeBlock(start: "./demo", port: 4830, mode: .session, runner: .binary),
            mcp: ["name": "demo-mcp", "path": "/api/mcp/sse", "autoRegister": true])
        XCTAssertEqual(validateManifest(m), [])
    }

    func testTheSilentFailureSpellingsAreCaught() {
        // A misspelled mode is the headline trap: it falls back to session.
        XCTAssertTrue(validateManifest([
            "id": "x", "runtime": ["kind": "server", "start": "./x", "mode": "deamon"],
        ]).contains { $0.contains("runtime.mode") })

        XCTAssertTrue(validateManifest([
            "id": "x", "runtime": ["kind": "server"],
        ]).contains { $0.contains("no `start`") })

        XCTAssertTrue(validateManifest([:]).contains { $0.contains("missing `id`") })

        XCTAssertTrue(validateManifest([
            "id": "x", "runtime": ["kind": "server", "start": "./x", "runner": "rust"],
        ]).contains { $0.contains("runtime.runner") })

        XCTAssertTrue(validateManifest([
            "id": "x", "sandbox": ["network": "hosts", "hosts": [] as [Any]],
        ]).contains { $0.contains("hosts") })

        XCTAssertTrue(validateManifest([
            "id": "x", "mcp": ["autoRegister": true],
        ]).contains { $0.contains("autoRegister") })
    }

    func testAnUnroutableLlmAdapterIsRejected() {
        // adapt: "local-mlx" would register the app and then never call it.
        XCTAssertTrue(validateManifest([
            "id": "x", "runtime": ["kind": "server", "start": "./x"],
            "llm": ["adapt": "local-mlx", "path": "/v1"],
        ]).contains { $0.contains("llm.adapt") })

        XCTAssertEqual(
            validateManifest([
                "id": "x", "runtime": ["kind": "server", "start": "./x"],
                "llm": ["adapt": "openai", "path": "/v1", "autoRegister": true],
            ]), [])
    }

    func testManifestJSONRoundTrips() throws {
        let m = manifest(id: "demo", name: "Demo", description: "d",
                         runtime: runtimeBlock(start: "./demo", port: 4830))
        let data = try manifestJSON(m)
        let back = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(back["id"] as? String, "demo")
        XCTAssertEqual((back["runtime"] as! [String: Any])["port"] as? Int, 4830)
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

private final class FakeDispatch: DispatchProvider {
    var finalized: [(String, Outcome)] = []
    func claimReady(_ capacity: Capacity) throws -> [WorkItem] {
        [WorkItem(id: "t1", prompt: "do it", dependsOn: ["t0"], timeoutSecs: 30)]
    }
    func finalize(_ itemId: String, _ outcome: Outcome) throws {
        finalized.append((itemId, outcome))
    }
}

final class DispatchTests: XCTestCase {
    func testWorkItemUsesSnakeCaseOnTheWire() {
        let item = WorkItem(id: "t1", prompt: "p", dependsOn: ["a"], timeoutSecs: 30)
        let j = item.json
        // camelCase would be silently dropped by the engine's serde — a
        // dependency that never held, not an error.
        XCTAssertNotNil(j["depends_on"])
        XCTAssertNotNil(j["timeout_secs"])
        XCTAssertNil(j["dependsOn"])
    }

    func testTaggedShapesSerialise() {
        XCTAssertEqual(Workspace.scratch.json["kind"] as? String, "scratch")
        XCTAssertEqual(Workspace.worktree(repo: "r", branch: "b").json["branch"] as? String, "b")
        XCTAssertEqual(McpServerSpec.http(name: "n", url: "u").json["transport"] as? String, "http")
        XCTAssertEqual(outcomeCompleted(summary: "done")["status"] as? String, "completed")
        XCTAssertEqual(outcomeTimedOut()["status"] as? String, "timed_out")
    }

    func testPollAndFinalizeRoundTrip() throws {
        let provider = FakeDispatch()
        let routes = dispatchRoutes(provider)

        let poll = routes[RouteKey("POST", "/api/dispatch/poll")]!
        let pollResp = poll(Request(method: "POST", path: "/api/dispatch/poll", query: [:], headers: [:],
                                    body: Data("{}".utf8)))
        let items = try JSONSerialization.jsonObject(with: responseData(pollResp)) as! [Any]
        XCTAssertEqual((items[0] as! [String: Any])["id"] as? String, "t1")

        let finalize = routes[RouteKey("POST", "/api/dispatch/finalize")]!
        let body = try JSONSerialization.data(withJSONObject: [
            "item_id": "t1", "outcome": ["status": "completed", "summary": "ok"],
        ])
        _ = finalize(Request(method: "POST", path: "/api/dispatch/finalize", query: [:], headers: [:], body: body))
        XCTAssertEqual(provider.finalized.first?.0, "t1")
    }

    private func responseData(_ r: Response) -> Data {
        if case let .data(d) = r.body { return d }
        return Data()
    }
}

// ---------------------------------------------------------------------------
// mcp
// ---------------------------------------------------------------------------

final class McpTests: XCTestCase {
    private func server() -> McpServer {
        let s = McpServer("demo-mcp")
        s.tool("demo_greet", "Greet someone",
               ["type": "object", "properties": ["name": ["type": "string"]]]) { args in
            "Hello, \((args["name"] as? String) ?? "?")"
        }
        return s
    }

    func testInitializeReportsServerInfo() {
        let r = server().handle(["jsonrpc": "2.0", "id": 1, "method": "initialize"])
        let info = (r["result"] as! [String: Any])["serverInfo"] as! [String: Any]
        XCTAssertEqual(info["name"] as? String, "demo-mcp")
    }

    func testToolsListCarriesTheSchema() {
        let r = server().handle(["id": 2, "method": "tools/list"])
        let tools = (r["result"] as! [String: Any])["tools"] as! [Any]
        let t = tools[0] as! [String: Any]
        XCTAssertEqual(t["name"] as? String, "demo_greet")
        XCTAssertNotNil(t["inputSchema"])
    }

    func testToolsCallRunsAndWrapsContent() {
        let r = server().handle([
            "id": 3, "method": "tools/call",
            "params": ["name": "demo_greet", "arguments": ["name": "Bến"]],
        ])
        let content = (r["result"] as! [String: Any])["content"] as! [Any]
        XCTAssertEqual((content[0] as! [String: Any])["text"] as? String, "Hello, Bến")
    }

    func testUnknownMethodIsMinus32601() {
        let r = server().handle(["id": 4, "method": "tools/teleport"])
        XCTAssertEqual(intOf((r["error"] as! [String: Any])["code"]), -32601)
    }

    func testErrorContentIsFlagged() {
        let c = errorContent("nope")
        XCTAssertEqual(c["isError"] as? Bool, true)
    }
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

final class AuthTests: XCTestCase {
    let token = "sca_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

    func testOnlyTheDaemonsOwnRequestPasses() {
        XCTAssertTrue(appTokenAuthorized(path: "/api/notes", presented: token, token: token, skip: []))
        // What the guard exists to stop: another local process on the port.
        XCTAssertFalse(appTokenAuthorized(path: "/api/notes", presented: nil, token: token, skip: []))
        XCTAssertFalse(appTokenAuthorized(path: "/api/notes", presented: "sca_wrong", token: token, skip: []))
    }

    func testExemptPathsMatchExactlyOrByPrefix() {
        let skip = ["/health", "/public/*"]
        XCTAssertTrue(appTokenAuthorized(path: "/health", presented: nil, token: token, skip: skip))
        XCTAssertTrue(appTokenAuthorized(path: "/public/logo.png", presented: nil, token: token, skip: skip))
        // A prefix must not leak into a sibling path.
        XCTAssertFalse(appTokenAuthorized(path: "/publicity", presented: nil, token: token, skip: skip))
    }

    func testWithoutAnIssuedTokenTheGuardIsInert() {
        // A bare `swift run`. Refusing everything would turn "not launched by
        // SenClaw" into "app is down".
        XCTAssertTrue(appTokenAuthorized(path: "/api/notes", presented: nil, token: nil, skip: []))
        XCTAssertTrue(appTokenAuthorized(path: "/api/notes", presented: nil, token: "", skip: []))
    }

    func testConstantTimeEquals() {
        XCTAssertTrue(constantTimeEquals("abc", "abc"))
        XCTAssertFalse(constantTimeEquals("abc", "abd"))
        XCTAssertFalse(constantTimeEquals("abc", "abcd"))
    }
}

// ---------------------------------------------------------------------------
// client construction (no network)
// ---------------------------------------------------------------------------

final class ClientTests: XCTestCase {
    func testExplicitConstructionTrimsTheBaseURL() throws {
        let c = try SpaceClient(appId: "demo", baseURL: "http://127.0.0.1:9/", appToken: "sca_x", apiVersion: 2)
        XCTAssertEqual(c.appId, "demo")
        XCTAssertEqual(c.baseURL, "http://127.0.0.1:9")
        XCTAssertEqual(c.appToken, "sca_x")
        XCTAssertEqual(c.apiVersion, 2)
    }

    func testUntypedJSONHelpers() {
        XCTAssertEqual(intOf(NSNumber(value: 5)), 5)
        XCTAssertEqual(intOf("nope"), 0)
        XCTAssertEqual(doubleOf(NSNumber(value: 1.5)), 1.5)
        XCTAssertEqual(pathEscape("a b/c"), "a%20b%2Fc")
    }
}
