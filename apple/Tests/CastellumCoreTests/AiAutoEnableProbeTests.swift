// Castellum - unit tests for the first-launch Ollama auto-enable decision
//. Pure logic only — no network, no Preferences, no AppModel.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AiAutoEnableProbeTests: XCTestCase {
    // MARK: shouldProbe

    func testShouldProbeOnlyWithNoStoredPreference() {
        XCTAssertTrue(AiAutoEnableProbe.shouldProbe(hasStoredAiEnabledPreference: false))
        XCTAssertFalse(AiAutoEnableProbe.shouldProbe(hasStoredAiEnabledPreference: true))
    }

    // MARK: isLoopback

    func testLoopbackHostsAreEligible() {
        XCTAssertTrue(AiAutoEnableProbe.isLoopback("http://127.0.0.1:11434"))
        XCTAssertTrue(AiAutoEnableProbe.isLoopback("http://localhost:11434"))
        XCTAssertTrue(AiAutoEnableProbe.isLoopback("http://[::1]:11434"))
    }

    func testNonLoopbackHostsAreNotEligible() {
        XCTAssertFalse(AiAutoEnableProbe.isLoopback("http://192.168.1.20:11434"))
        XCTAssertFalse(AiAutoEnableProbe.isLoopback("http://ollama.example.com:11434"))
        XCTAssertFalse(AiAutoEnableProbe.isLoopback("not a url"))
    }

    // MARK: decide — the injected-probe-result unit test asks for

    func testDecideEnablesWhenTheLoopbackProbeSucceeds() {
        let decision = AiAutoEnableProbe.decide(
            endpoint: AiAutoEnableProbe.probeEndpoint, probeSucceeded: true)
        guard case .enable(let endpoint, let reason, let detected) = decision else {
            return XCTFail("expected .enable, got \(decision)")
        }
        XCTAssertEqual(endpoint, AiAutoEnableProbe.probeEndpoint)
        XCTAssertEqual(detected, AiAutoEnableProbe.probeEndpoint)
        XCTAssertEqual(
            reason, "Turned on because Ollama was found on this Mac; nothing leaves it.")
    }

    func testDecideDoesNotEnableWhenTheProbeFails() {
        let decision = AiAutoEnableProbe.decide(
            endpoint: AiAutoEnableProbe.probeEndpoint, probeSucceeded: false)
        XCTAssertEqual(decision, .doNotEnable)
    }

    /// The invariant that matters most: even a (hypothetical) successful probe
    /// against a non-loopback endpoint must never enable AI automatically.
    func testDecideNeverEnablesForANonLoopbackEndpointEvenIfItAnswered() {
        let decision = AiAutoEnableProbe.decide(
            endpoint: "http://192.168.1.20:11434", probeSucceeded: true)
        XCTAssertEqual(decision, .doNotEnable)
    }
}
