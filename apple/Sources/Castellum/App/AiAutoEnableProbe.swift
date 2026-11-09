// Castellum - first-launch Ollama auto-enable decision.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure decision logic, extracted so the "never auto-enable a non-loopback
// endpoint" invariant is unit-testable without a real network call. The
// network probe itself lives on AppModel (bootstrap-time, off-actor); this
// type only decides what a probe RESULT should do to Preferences.

import Foundation

enum AiAutoEnableProbe {
    /// The one endpoint the first-launch probe ever checks. Never derived
    /// from user input — that is what makes "never auto-enable a
    /// non-loopback endpoint" true by construction rather than by a runtime
    /// check that could be bypassed.
    static let probeEndpoint = "http://127.0.0.1:11434"
    /// Timeout for the probe request, per.
    static let probeTimeoutSeconds: TimeInterval = 2.0

    /// Only run the probe once per install: when there is no stored
    /// `aiEnabled` preference at all (a fresh install / fresh defaults
    /// domain), not merely when it happens to be `false` today.
    static func shouldProbe(hasStoredAiEnabledPreference: Bool) -> Bool {
        !hasStoredAiEnabledPreference
    }

    /// Whether `endpoint` is eligible for auto-enable at all — loopback only.
    /// Belt-and-braces: `probeEndpoint` above is already fixed to loopback,
    /// but a decision that re-validates this itself can't silently drift if
    /// the probe target is ever parameterised later.
    static func isLoopback(_ endpoint: String) -> Bool {
        guard let host = URL(string: endpoint)?.host else { return false }
        return host == "127.0.0.1" || host == "localhost" || host == "::1"
    }

    /// The outcome of a completed first-launch probe.
    enum Decision: Equatable {
        /// Turn AI on, pointed at `endpoint`, with `reason` shown under the
        /// toggle and `detectedEndpoint` shown as the endpoint field's caption.
        case enable(endpoint: String, reason: String, detectedEndpoint: String)
        /// Leave AI off; the user can still enable it manually.
        case doNotEnable
    }

    /// Pure decision for one probe attempt. `probeSucceeded` is the caller's
    /// off-actor network result; a non-loopback endpoint is refused even if
    /// (hypothetically) `probeSucceeded` were true for one.
    static func decide(endpoint: String, probeSucceeded: Bool) -> Decision {
        guard probeSucceeded, isLoopback(endpoint) else { return .doNotEnable }
        return .enable(
            endpoint: endpoint,
            reason: "Turned on because Ollama was found on this Mac; nothing leaves it.",
            detectedEndpoint: endpoint
        )
    }
}
