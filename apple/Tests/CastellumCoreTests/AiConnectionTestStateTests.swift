// Castellum - Tests for AiConnectionTestState (pure Settings "Test Connection"
// state machine: success/failure indicator + clear-on-change rules).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AiConnectionTestStateTests: XCTestCase {
    private func model(_ name: String) -> AiModelInfo {
        AiModelInfo(name: name, sizeBytes: nil)
    }

    // MARK: - Picker names (FP37)

    func testPersistedModelIsListedBeforeAnyTest() {
        let state = AiConnectionTestState()
        XCTAssertEqual(state.pickerModelNames(current: ["llama3", ""]), ["llama3"])
    }

    func testFetchedNamesComeFirstWithoutDuplicates() {
        var state = AiConnectionTestState()
        state.testSucceeded(models: [model("llama3"), model("mistral")])
        XCTAssertEqual(state.pickerModelNames(current: ["mistral", "embed"]), ["llama3", "mistral", "embed"])
    }

    func testEndpointChangeKeepsThePersistedName() {
        var state = AiConnectionTestState()
        state.testSucceeded(models: [model("llama3")])
        state.endpointChanged()
        XCTAssertEqual(state.pickerModelNames(current: ["llama3"]), ["llama3"])
    }

    // MARK: - Success / failure transitions

    func testTestStartedClearsAnyPriorResult() {
        var state = AiConnectionTestState()
        state.testSucceeded(models: [model("llama3")])
        state.testStarted()
        XCTAssertTrue(state.isTesting)
        XCTAssertNil(state.testError)
        XCTAssertFalse(state.testSucceeded)
        XCTAssertTrue(state.models.isEmpty)
    }

    func testSuccessSetsModelsAndClearsError() {
        var state = AiConnectionTestState()
        state.testStarted()
        state.testFailed(message: "stale error from a previous attempt")
        state.testSucceeded(models: [model("llama3"), model("mistral")])
        XCTAssertFalse(state.isTesting)
        XCTAssertTrue(state.testSucceeded)
        XCTAssertNil(state.testError)
        XCTAssertEqual(state.models.map(\.name), ["llama3", "mistral"])
    }

    func testFailurePreservesTheVerbatimMessageAndClearsSuccess() {
        var state = AiConnectionTestState()
        state.testStarted()
        state.testSucceeded(models: [model("llama3")])
        // The egress-refused privacy text must survive unmodified.
        let verbatim = "Local network only — public endpoints are refused by the core."
        state.testFailed(message: verbatim)
        XCTAssertFalse(state.isTesting)
        XCTAssertFalse(state.testSucceeded)
        XCTAssertEqual(state.testError, verbatim)
    }

    // MARK: - Clear-on-change rules

    func testEndpointChangeClearsResultAndModelList() {
        var state = AiConnectionTestState()
        state.testSucceeded(models: [model("llama3")])
        state.endpointChanged()
        XCTAssertNil(state.testError)
        XCTAssertFalse(state.testSucceeded)
        XCTAssertTrue(state.models.isEmpty, "a different endpoint may serve a different model list")
    }

    func testEndpointChangeAlsoClearsAFailure() {
        var state = AiConnectionTestState()
        state.testFailed(message: "endpoint refused")
        state.endpointChanged()
        XCTAssertNil(state.testError)
    }

    func testModelChangeClearsTheIndicatorButKeepsTheModelList() {
        var state = AiConnectionTestState()
        let fetched = [model("llama3"), model("mistral")]
        state.testSucceeded(models: fetched)
        state.modelChanged()
        XCTAssertNil(state.testError)
        XCTAssertFalse(state.testSucceeded)
        XCTAssertEqual(
            state.models, fetched,
            "picking a model must not blank the picker's own data source"
        )
    }
}
