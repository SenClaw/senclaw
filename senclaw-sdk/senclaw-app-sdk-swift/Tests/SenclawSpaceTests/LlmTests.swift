// The LLM-provider contract, driven end to end. The bytes a client would see
// are the bytes these tests inspect — SenClaw's OpenAI adapter accumulates
// `delta.tool_calls[].function.{name,arguments}` by CONCATENATION keyed on
// `index`, so a second call reusing index 0 produces `get_weatherget_time`, and
// no test of the event builder in isolation would notice.

import Foundation
import XCTest

@testable import SenclawSpace

private struct Fake: LlmProvider {
    let chunks: [Chunk]
    var id = "m"
    func models() -> [ModelCard] {
        [ModelCard(id, contextLength: 4096, maxOutputTokens: 512, vision: false)]
    }
    func chat(_ req: ChatRequest, _ sink: ChunkSink) throws {
        for c in chunks { sink.send(c) }
    }
}

private struct Boom: LlmProvider {
    func models() -> [ModelCard] { [ModelCard("m", contextLength: 4096, maxOutputTokens: 512, vision: false)] }
    func chat(_ req: ChatRequest, _ sink: ChunkSink) throws {
        sink.text("partial")
        throw SenclawError("provider exploded")
    }
}

extension Response {
    /// The buffered body, for asserting on a non-stream reply.
    var testData: Data? { if case let .data(d) = body { return d }; return nil }
    var isStream: Bool { if case .stream = body { return true }; return false }
}

private func chatReq(_ body: [String: Any]) throws -> ChatRequest { try ChatRequest.fromBody(body) }

private func simpleChat(stream: Bool, model: String = "m") -> ChatRequest {
    try! ChatRequest.fromBody([
        "model": model, "messages": [["role": "user", "content": "hi"]], "stream": stream,
    ])
}

private func collectStream(_ provider: LlmProvider, _ chat: ChatRequest) -> [String] {
    var out: [String] = []
    runStream(provider, chat, emit: { out.append($0) })
    return out
}

private func ssePayloads(_ raw: [String]) -> [[String: Any]] {
    raw.filter { $0 != "[DONE]" }.compactMap {
        (try? JSONSerialization.jsonObject(with: Data($0.utf8))) as? [String: Any]
    }
}

private func postChat(_ provider: LlmProvider, _ body: [String: Any]) -> Response {
    let handler = llmRoutes(provider)[RouteKey("POST", "/v1/chat/completions")]!
    let req = Request(
        method: "POST", path: "/v1/chat/completions", query: [:], headers: [:],
        body: try! JSONSerialization.data(withJSONObject: body))
    return handler(req)
}

final class LlmTests: XCTestCase {
    func testRequestWithoutMessagesIsRefused() {
        XCTAssertThrowsError(try chatReq(["model": "m"]))
        XCTAssertThrowsError(try chatReq(["model": "m", "messages": []]))
        XCTAssertThrowsError(try chatReq(["messages": [["role": "user"]]]))
    }

    func testBothSpellingsOfTheOutputCeilingAreRead() throws {
        let msgs: [Any] = [["role": "user", "content": "hi"]]
        let a = try chatReq(["model": "m", "messages": msgs, "max_tokens": 100])
        XCTAssertEqual(a.maxTokens, 100)
        // The newer spelling wins when a client sends both.
        let b = try chatReq([
            "model": "m", "messages": msgs, "max_tokens": 100, "max_completion_tokens": 200,
        ])
        XCTAssertEqual(b.maxTokens, 200)
    }

    func testMultipartContentSurvivesTheRequestParse() throws {
        let req = try chatReq([
            "model": "m",
            "messages": [[
                "role": "user",
                "content": [
                    ["type": "text", "text": "what is this"],
                    ["type": "image_url", "image_url": ["url": "data:image/png;base64,AAA"]],
                ],
            ]],
        ])
        let parts = (req.messages[0] as! [String: Any])["content"] as! [Any]
        XCTAssertEqual(parts.count, 2)
        let img = (parts[1] as! [String: Any])["image_url"] as! [String: Any]
        XCTAssertEqual(img["url"] as? String, "data:image/png;base64,AAA")
    }

    func testEmptyTextEmitsNoChunkAtAll() {
        var index = 0
        XCTAssertNil(renderChunk("id", "m", .text(""), &index))
        XCTAssertNil(renderChunk("id", "m", .reasoning(""), &index))
    }

    func testTwoToolCallsStreamAtDistinctIndices() {
        let raw = collectStream(
            Fake(chunks: [
                .toolCall(id: "call_a", name: "get_weather", arguments: #"{"city":"Hanoi"}"#),
                .toolCall(id: "call_b", name: "get_time", arguments: "{}"),
            ]), simpleChat(stream: true))

        let calls = ssePayloads(raw).compactMap {
            ((($0["choices"] as? [Any])?.first as? [String: Any])?["delta"] as? [String: Any])?["tool_calls"] as? [Any]
        }.compactMap { $0.first as? [String: Any] }

        XCTAssertEqual(calls.count, 2)
        XCTAssertEqual(intOf(calls[0]["index"]), 0)
        XCTAssertEqual((calls[0]["function"] as! [String: Any])["name"] as? String, "get_weather")
        // A reused index welds the two calls together.
        XCTAssertEqual(intOf(calls[1]["index"]), 1)
        XCTAssertEqual((calls[1]["function"] as! [String: Any])["name"] as? String, "get_time")
    }

    func testAStreamAlwaysTerminatesWithDone() {
        let raw = collectStream(Fake(chunks: [.text("hi")]), simpleChat(stream: true))
        XCTAssertEqual(raw.last, "[DONE]", "an unterminated stream reads as a hang, not a failure")
    }

    func testUsageArrivesOnItsOwnChunkWithNoChoices() {
        let raw = collectStream(
            Fake(chunks: [.text("hi"), .usage(promptTokens: 12, completionTokens: 3)]),
            simpleChat(stream: true))
        let usage = ssePayloads(raw).first { $0["usage"] != nil }!
        let u = usage["usage"] as! [String: Any]
        XCTAssertEqual(intOf(u["prompt_tokens"]), 12)
        XCTAssertEqual(intOf(u["total_tokens"]), 15)
        XCTAssertEqual((usage["choices"] as! [Any]).count, 0)
    }

    func testAFailedGenerationEndsWithAnErrorChunkThenDone() {
        let raw = collectStream(Boom(), simpleChat(stream: true))
        XCTAssertEqual(raw.last, "[DONE]")
        let err = ssePayloads(raw).first { $0["error"] != nil }!
        let choice = (err["choices"] as! [Any]).first as! [String: Any]
        XCTAssertEqual(choice["finish_reason"] as? String, "error")
    }

    func testNonStreamReturnsOneAssembledMessage() {
        let resp = postChat(
            Fake(chunks: [.reasoning("thinking"), .text("he"), .text("llo")]),
            ["model": "m", "messages": [["role": "user", "content": "hi"]], "stream": false])
        let body = try! JSONSerialization.jsonObject(with: resp.testData!) as! [String: Any]
        let msg = ((body["choices"] as! [Any])[0] as! [String: Any])["message"] as! [String: Any]
        XCTAssertEqual(msg["content"] as? String, "hello", "text deltas must be concatenated")
        XCTAssertEqual(msg["reasoning_content"] as? String, "thinking")
        XCTAssertEqual(body["object"] as? String, "chat.completion")
    }

    func testFinishReasonIsToolCallsWhenThereAreCalls() {
        let calls: [[String: Any]] = [["id": "c1"]]
        let withCalls = nonStreamBody("m", "", "", calls, nil)
        XCTAssertEqual(finishReason(withCalls), "tool_calls")
        let plain = nonStreamBody("m", "hi", "", [], nil)
        XCTAssertEqual(finishReason(plain), "stop")
    }

    func testReasoningIsOmittedWhenEmpty() {
        let a = nonStreamBody("m", "hi", "", [], nil)
        XCTAssertNil(message(a)["reasoning_content"])
        let b = nonStreamBody("m", "hi", "why", [], nil)
        XCTAssertEqual(message(b)["reasoning_content"] as? String, "why")
    }

    func testAnUnknownModelIs404NotASilentDefault() {
        let resp = postChat(Fake(chunks: []), ["model": "nope", "messages": [["role": "user"]]])
        XCTAssertEqual(resp.status, 404)
    }

    func testModelsEndpointCarriesTheCapabilityFields() {
        let handler = llmRoutes(Fake(chunks: []))[RouteKey("GET", "/v1/models")]!
        let resp = handler(Request(method: "GET", path: "/v1/models", query: [:], headers: [:], body: Data()))
        let body = try! JSONSerialization.jsonObject(with: resp.testData!) as! [String: Any]
        let m = (body["data"] as! [Any])[0] as! [String: Any]
        XCTAssertEqual(m["id"] as? String, "m")
        XCTAssertEqual(intOf(m["context_length"]), 4096)
        XCTAssertEqual(m["vision"] as? Bool, false)
    }

    func testAnEmptyModelListNeverClobbersAGoodCache() throws {
        let dir = NSTemporaryDirectory() + "senclaw-swift-test-\(UUID().uuidString)"
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let card = ModelCard("m", contextLength: 4096, maxOutputTokens: 512, vision: false)
        try publishModels(dir, [card])
        let good = try String(contentsOf: cachePath(dir), encoding: .utf8)

        XCTAssertThrowsError(try publishModels(dir, []))
        let after = try String(contentsOf: cachePath(dir), encoding: .utf8)
        XCTAssertEqual(good, after, "a failed publish must leave the cache intact")
    }

    func testPublishedCardsRoundTrip() throws {
        let dir = NSTemporaryDirectory() + "senclaw-swift-test-\(UUID().uuidString)"
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let m = ModelCard("gemma", contextLength: 128_000, maxOutputTokens: 8192, vision: true, displayName: "Gemma 4")
        try publishModels(dir, [m])
        let back = try JSONSerialization.jsonObject(with: Data(contentsOf: cachePath(dir))) as! [String: Any]
        let cards = (back["models"] as! [Any]).map { ModelCard.fromWire($0 as! [String: Any])! }
        XCTAssertEqual(cards[0].id, "gemma")
        XCTAssertEqual(cards[0].displayName, "Gemma 4")
        XCTAssertTrue(cards[0].vision)
        XCTAssertTrue(cards[0].tools, "tools defaults to true when absent")
    }

    // -- helpers ----------------------------------------------------------

    private func message(_ body: [String: Any]) -> [String: Any] {
        ((body["choices"] as! [Any])[0] as! [String: Any])["message"] as! [String: Any]
    }
    private func finishReason(_ body: [String: Any]) -> String {
        ((body["choices"] as! [Any])[0] as! [String: Any])["finish_reason"] as! String
    }
    private func cachePath(_ dir: String) -> URL {
        URL(fileURLWithPath: dir).appendingPathComponent(MODELS_CACHE_PATH)
    }
}
