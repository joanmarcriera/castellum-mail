// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

///: a failed send must name the SMTP endpoint and classify the
/// reason with the same ErrorKind vocabulary AccountSyncStatus uses for sync
/// failures. `castellum-core/src/send.rs` records `"<kind>: <detail>"`
/// (kind is a closed, Rust-controlled vocabulary; detail is local-only debug
/// text) — this only ever reads the kind token, matching
/// `AccountSyncStatus.message`'s stance of never surfacing raw text.
final class SendFailurePresentationTests: XCTestCase {
    private let endpoint = "smtp.example.test:587"

    func testCoreKindsMapToDistinctEndpointNamingMessages() {
        let expected: [(String, [String])] = [
            ("network: connection refused", ["Couldn’t reach", endpoint]),
            ("auth: bad credentials", ["Sign-in to", endpoint]),
            ("tls: handshake failed", ["Secure connection to", endpoint]),
            ("protocol: unexpected response", [endpoint, "unexpected response"]),
            ("unsupported: no STARTTLS", [endpoint, "doesn’t support"]),
        ]
        for (raw, mustContain) in expected {
            let reason = SendFailurePresentation.reason(for: raw, endpoint: endpoint)
            for fragment in mustContain {
                XCTAssertTrue(
                    reason.contains(fragment),
                    "kind \(raw): expected \(reason.debugDescription) to contain \(fragment.debugDescription)"
                )
            }
        }
    }

    func testUnclassifiedOrMissingPrefixFallsBackToGenericMessage() {
        for raw in ["SMTP delivery failed.", "SMTP delivery failed — will retry later.", ""] {
            let reason = SendFailurePresentation.reason(for: raw, endpoint: endpoint)
            XCTAssertTrue(reason.contains(endpoint), "expected the endpoint even when unclassified")
            XCTAssertTrue(reason.contains("failed"))
        }
    }

    func testDetailAfterTheKindPrefixNeverReachesTheMessage() {
        // A hostile/leaky detail (e.g. server-echoed text) must never surface
        // in the presented reason — only the closed-vocabulary kind may.
        let hostile = "auth: click here to verify your account: evil.example/phish"
        let reason = SendFailurePresentation.reason(for: hostile, endpoint: endpoint)
        XCTAssertFalse(reason.contains("evil.example"))
        XCTAssertFalse(reason.contains("click here"))
    }

    func testOutboxMessagePrefixesNotSentKeptInOutbox() {
        let message = SendFailurePresentation.outboxMessage(
            rawReason: "network: connection refused", endpoint: endpoint)
        XCTAssertTrue(message.hasPrefix("Not sent — kept in Outbox."))
        XCTAssertTrue(message.contains(endpoint))
    }

    func testNilEndpointFallsBackToGenericWording() {
        let reason = SendFailurePresentation.reason(for: "network: x", endpoint: nil)
        XCTAssertTrue(reason.contains("the SMTP server"))
    }
}
