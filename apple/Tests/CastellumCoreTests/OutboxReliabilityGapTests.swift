// Castellum - Outbox wording for a permanent refusal and a failed Sent copy (G8, G9).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
//. The core now records a permanent 5xx refusal of a message
// as `rejected: <server text>` and stops retrying it; the app must not
// promise an automatic retry or call it a sign-in failure. A failed
// `sent_copy` op must read as "sent, copy missing", not as an unsent message.
// Pure presentation tests.

import XCTest
@testable import Castellum

@MainActor
final class OutboxReliabilityGapTests: XCTestCase {
    private let rejected = "rejected: permanent error (550): 5.1.1 Recipient address rejected: User unknown"

    func testARejectedSendSaysTheServerRefusedItAndPromisesNoAutomaticRetry() {
        let caption = SendFailurePresentation.outboxMessage(rawReason: rejected, endpoint: "smtp.example.com:587")
        XCTAssertTrue(caption.contains("refused this message"), caption)
        XCTAssertTrue(caption.contains("Retry"), caption)
        XCTAssertFalse(caption.localizedCaseInsensitiveContains("retry automatically"), caption)
        XCTAssertFalse(caption.contains("5.1.1"), "the server's text stays out of the caption")
    }

    func testTheBackgroundNoticeDoesNotCallARejectionASignInFailure() {
        XCTAssertEqual(SendFailureNotice.reason(for: rejected), "The mail server refused the message.")
    }

    func testAFailedSentCopySaysTheMessageWasSent() {
        let note = try? XCTUnwrap(outboxSentCopyNote(kind: "sent_copy", status: "failed"))
        XCTAssertTrue(note?.contains("was sent") ?? false)
        XCTAssertNotNil(outboxSentCopyNote(kind: "sent_copy", status: "failed_permanent"))
        XCTAssertNil(outboxSentCopyNote(kind: "sent_copy", status: "queued"))
        XCTAssertNil(outboxSentCopyNote(kind: "send", status: "failed"))
    }
}
