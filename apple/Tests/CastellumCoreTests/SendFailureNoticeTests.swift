// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// FP29: the outcome-to-notice mapping for a send that fails after the undo
/// window. Pure; no core, Keychain or server.
final class SendFailureNoticeTests: XCTestCase {
    func testNothingFailedGivesNoNotice() {
        XCTAssertNil(SendFailureNotice.message(failed: 0, thrown: nil, lastError: nil))
    }

    func testFailedSendNamesOutboxAndHidesRawProviderText() throws {
        let raw = "smtp: connection refused 127.0.0.1:3998 (os error 61)"
        let m = try XCTUnwrap(SendFailureNotice.message(failed: 1, thrown: nil, lastError: raw))
        XCTAssertTrue(m.hasPrefix("Not sent"), m)
        XCTAssertTrue(m.contains("Outbox"), m)
        XCTAssertFalse(m.contains("127.0.0.1"), m)
        XCTAssertFalse(m.contains("os error"), m)
    }

    func testThrownErrorAlsoGivesNotice() {
        XCTAssertNotNil(SendFailureNotice.message(failed: 0, thrown: "bad config", lastError: nil))
    }

    func testReasonKinds() {
        XCTAssertEqual(SendFailureNotice.reason(for: "535 5.7.8 Authentication failed"),
                       "The mail server refused the sign-in.")
        XCTAssertEqual(SendFailureNotice.reason(for: "connection timed out"),
                       "The mail server could not be reached.")
        XCTAssertEqual(SendFailureNotice.reason(for: "552 mailbox full"),
                       "The mail server did not accept the message.")
    }
}
