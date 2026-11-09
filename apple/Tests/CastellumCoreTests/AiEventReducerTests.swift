// Castellum - Tests for AiEventReducer (pure streaming delta accumulator).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AiEventReducerTests: XCTestCase {
    // Convenience: make a fresh AiJobViewState for tests.
    private func makeState(id: Int64 = 1) -> AiJobViewState {
        AiJobViewState(id: id, feature: "summarize_message", ownerId: "msg:1",
                       status: "queued", text: "", errorCode: nil,
                       isCached: false, lastEventId: 0)
    }

    // Convenience: make an AiEvent.
    private func event(id: Int64, kind: String, payload: String = "") -> AiEvent {
        AiEvent(id: id, kind: kind, payload: payload)
    }

    // MARK: - single-event behaviours

    func testStartedResetsTextAndSetsRunning() {
        var state = makeState()
        state.text = "previous text that should be cleared"
        state.status = "queued"
        AiEventReducer.apply(event: event(id: 1, kind: "started"), to: &state)
        XCTAssertEqual(state.text, "")
        XCTAssertEqual(state.status, "running")
        XCTAssertEqual(state.lastEventId, 1)
    }

    func testDeltaAppendsText() {
        var state = makeState()
        AiEventReducer.apply(event: event(id: 1, kind: "delta", payload: "Hello "), to: &state)
        AiEventReducer.apply(event: event(id: 2, kind: "delta", payload: "world"), to: &state)
        XCTAssertEqual(state.text, "Hello world")
        XCTAssertEqual(state.lastEventId, 2)
    }

    func testDeltaDecodesExecutorJSONPayloadWithoutShowingJSON() {
        var state = makeState()
        AiEventReducer.apply(
            event: event(id: 1, kind: "delta", payload: #"{"text":"First line\n"}"#),
            to: &state
        )
        AiEventReducer.apply(
            event: event(id: 2, kind: "delta", payload: #"{"text":"Second line"}"#),
            to: &state
        )
        XCTAssertEqual(state.text, "First line\nSecond line")
        XCTAssertFalse(state.text.contains(#"{"text"#))
    }

    func testSucceededFinalizesStatus() {
        var state = makeState()
        state.status = "running"
        AiEventReducer.apply(event: event(id: 5, kind: "succeeded"), to: &state)
        XCTAssertEqual(state.status, "succeeded")
        XCTAssertTrue(state.isTerminal)
    }

    func testFailedSetsStatusAndErrorCode() {
        var state = makeState()
        let payload = #"{"error_code":"model_missing"}"#
        AiEventReducer.apply(event: event(id: 3, kind: "failed", payload: payload), to: &state)
        XCTAssertEqual(state.status, "failed")
        XCTAssertEqual(state.errorCode, "model_missing")
        XCTAssertTrue(state.isTerminal)
    }

    func testErrorEventFallsBackToRawPayload() {
        var state = makeState()
        AiEventReducer.apply(event: event(id: 4, kind: "error", payload: "endpoint_refused"), to: &state)
        XCTAssertEqual(state.status, "failed")
        XCTAssertEqual(state.errorCode, "endpoint_refused")
    }

    func testCancelledSetsStatus() {
        var state = makeState()
        AiEventReducer.apply(event: event(id: 7, kind: "cancelled"), to: &state)
        XCTAssertEqual(state.status, "cancelled")
        XCTAssertTrue(state.isTerminal)
    }

    func testUnknownEventKindIsIgnored() {
        var state = makeState()
        state.status = "running"
        state.text = "some text"
        AiEventReducer.apply(event: event(id: 1, kind: "unknown_future_kind"), to: &state)
        XCTAssertEqual(state.status, "running")
        XCTAssertEqual(state.text, "some text")
        XCTAssertEqual(state.lastEventId, 1)   // id still advances
    }

    // MARK: - batch behaviours

    func testBatchAccumulatesDeltasInOrder() {
        var state = makeState()
        let events = [
            event(id: 1, kind: "started"),
            event(id: 2, kind: "delta", payload: "The "),
            event(id: 3, kind: "delta", payload: "cat sat."),
            event(id: 4, kind: "succeeded"),
        ]
        AiEventReducer.applyBatch(events: events, to: &state)
        XCTAssertEqual(state.text, "The cat sat.")
        XCTAssertEqual(state.status, "succeeded")
        XCTAssertTrue(state.isTerminal)
        XCTAssertEqual(state.lastEventId, 4)
    }

    func testStartedMidStreamResetsText() {
        // If the executor retries a job, a second `started` event arrives.
        // The accumulated text must reset so the UI doesn't show stale content.
        var state = makeState()
        let events = [
            event(id: 1, kind: "started"),
            event(id: 2, kind: "delta", payload: "stale partial text"),
            event(id: 3, kind: "started"),   // retry — reset expected
            event(id: 4, kind: "delta", payload: "fresh start"),
        ]
        AiEventReducer.applyBatch(events: events, to: &state)
        XCTAssertEqual(state.text, "fresh start")
        XCTAssertEqual(state.status, "running")
        XCTAssertFalse(state.isTerminal)
    }

    func testLastEventIdAdvancesMonotonically() {
        var state = makeState()
        AiEventReducer.apply(event: event(id: 10, kind: "delta", payload: "a"), to: &state)
        XCTAssertEqual(state.lastEventId, 10)
        // Lower id (out-of-order / duplicate) should NOT regress the high-water mark.
        AiEventReducer.apply(event: event(id: 5, kind: "delta", payload: "b"), to: &state)
        XCTAssertEqual(state.lastEventId, 10)
        AiEventReducer.apply(event: event(id: 11, kind: "delta", payload: "c"), to: &state)
        XCTAssertEqual(state.lastEventId, 11)
    }

    func testIsTerminalForAllTerminalStatuses() {
        let terminalStatuses = ["succeeded", "failed", "cancelled"]
        for status in terminalStatuses {
            var state = makeState()
            state.status = status
            XCTAssertTrue(state.isTerminal, "\(status) should be terminal")
        }
        let nonTerminal = ["queued", "leased", "running", "streaming"]
        for status in nonTerminal {
            var state = makeState()
            state.status = status
            XCTAssertFalse(state.isTerminal, "\(status) should not be terminal")
        }
    }

    // MARK: - cache decision (Regenerate / forceRefresh semantics)

    private func output(complete: Bool) -> AiOutput {
        AiOutput(text: "cached summary", complete: complete, model: "llama3.1:8b",
                 tokensIn: 100, tokensOut: 50, jobId: 1)
    }

    func testCompleteCachedOutputIsUsed() {
        XCTAssertTrue(AiEventReducer.shouldUseCachedOutput(
            output(complete: true), forceRefresh: false))
    }

    func testForceRefreshBypassesCompleteCache() {
        // Regenerate must skip the cache even when a complete output exists,
        // otherwise it's a no-op that re-serves the same cached text forever.
        XCTAssertFalse(AiEventReducer.shouldUseCachedOutput(
            output(complete: true), forceRefresh: true))
    }

    func testPartialCachedOutputIsNotUsed() {
        // A cancelled/interrupted run stores complete=false; never auto-serve it.
        XCTAssertFalse(AiEventReducer.shouldUseCachedOutput(
            output(complete: false), forceRefresh: false))
    }

    func testMissingCachedOutputIsNotUsed() {
        XCTAssertFalse(AiEventReducer.shouldUseCachedOutput(nil, forceRefresh: false))
        XCTAssertFalse(AiEventReducer.shouldUseCachedOutput(nil, forceRefresh: true))
    }

    // MARK: - shared error message helper

    func testErrorMessageModelMissing() {
        let msg = AiEventReducer.errorMessage(code: "model_missing", modelName: "llama3.1:8b")
        XCTAssertTrue(msg.contains("llama3.1:8b"), "model name should appear in the message")
        XCTAssertTrue(msg.contains("not installed"), "should indicate the model is missing")
    }

    func testErrorMessageEndpointRefused() {
        let msg = AiEventReducer.errorMessage(code: "endpoint_refused", modelName: "any")
        XCTAssertTrue(msg.lowercased().contains("local network"),
                      "should mention local network restriction")
    }

    func testErrorMessageNetwork() {
        let msg = AiEventReducer.errorMessage(code: "network", modelName: "any")
        XCTAssertTrue(msg.contains("Ollama"), "should mention Ollama for network errors")
    }

    func testErrorMessageTimeout() {
        let msg = AiEventReducer.errorMessage(code: "timeout", modelName: "any")
        XCTAssertTrue(msg.lowercased().contains("timed out") || msg.lowercased().contains("timeout"),
                      "should indicate a timeout")
    }

    func testErrorMessageUnknownCodeReturnsCode() {
        let msg = AiEventReducer.errorMessage(code: "some_unknown_error", modelName: "any")
        XCTAssertEqual(msg, "some_unknown_error",
                       "unknown codes should be returned verbatim")
    }

    func testErrorMessageNilCodeReturnsFallback() {
        let msg = AiEventReducer.errorMessage(code: nil, modelName: "any")
        XCTAssertEqual(msg, "AI request failed",
                       "nil code should produce the generic fallback")
    }

    // MARK: - latency telemetry (M5)

    private func decodePoll(_ json: String) throws -> AiPollResult {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(AiPollResult.self, from: Data(json.utf8))
    }

    func testOldPollJSONWithoutTimingFieldsStillDecodes() throws {
        let old = #"{"job":{"id":3,"status":"succeeded","feature":"summarize_message","attempts":1,"error_code":null},"events":[]}"#
        let result = try decodePoll(old)
        XCTAssertNil(result.job.durationMs)
        XCTAssertNil(result.job.tokensPerSecond)
        var state = makeState()
        AiEventReducer.applyTiming(from: result.job, to: &state)
        XCTAssertNil(state.durationMs)
        XCTAssertNil(state.tokensPerSecond)
    }

    func testNewPollJSONCarriesTimingIntoState() throws {
        let json = #"{"job":{"id":3,"status":"succeeded","feature":"summarize_message","attempts":1,"error_code":null,"duration_ms":500,"tokens_in":42,"tokens_out":10,"tokens_per_second":20.0},"events":[]}"#
        let result = try decodePoll(json)
        var state = makeState()
        AiEventReducer.applyTiming(from: result.job, to: &state)
        XCTAssertEqual(state.durationMs, 500)
        XCTAssertEqual(state.tokensPerSecond, 20.0)
    }

    func testCompletedEventPayloadCarriesTiming() {
        var state = makeState()
        AiEventReducer.apply(
            event: event(id: 5, kind: "completed",
                         payload: #"{"duration_ms":1200,"tokens_per_second":18.4,"tokens_out":22}"#),
            to: &state)
        XCTAssertEqual(state.durationMs, 1200)
        XCTAssertEqual(state.tokensPerSecond, 18.4)
        // An older core's bare "{}" leaves the fields untouched.
        var old = makeState()
        AiEventReducer.apply(event: event(id: 1, kind: "completed", payload: "{}"), to: &old)
        XCTAssertNil(old.durationMs)
    }

    func testTimingLabelShowsDurationAndOptionalRate() {
        XCTAssertNil(AiEventReducer.timingLabel(durationMs: nil, tokensPerSecond: 5))
        XCTAssertEqual(AiEventReducer.timingLabel(durationMs: 1234, tokensPerSecond: nil), "1.2 s")
        let both = AiEventReducer.timingLabel(durationMs: 500, tokensPerSecond: 20)
        XCTAssertTrue(both?.hasPrefix("0.5 s · 20.0") ?? false, both ?? "nil")
    }
}
