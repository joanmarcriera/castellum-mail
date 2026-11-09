// Castellum - AI Activity row labels (RP03).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AiActivityLabelsTests: XCTestCase {
    private func header(_ id: String, subject: String?) -> MessageHeader {
        MessageHeader(id: id, accountId: "a", mailboxId: "a:INBOX",
                      subject: subject, fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
    }

    func testThreadExtractionJobGetsAReadableKind() {
        let label = AiActivityLabels.kind(forFeature: "extract_thread_items")
        XCTAssertFalse(label.contains("_"), "raw identifier leaked: \(label)")
    }

    func testThreadOwnerResolvesToTheRootMessageSubject() {
        let msgs = [header("alice@local.test:INBOX:119", subject: "Quarterly plan")]
        XCTAssertEqual(
            AiActivityLabels.subject(forOwnerId: "t:alice@local.test:INBOX:119", in: msgs),
            "Quarterly plan")
    }

    func testMessageOwnerStillResolvesAndUnknownFallsBack() {
        let msgs = [header("m1", subject: "Hello")]
        XCTAssertEqual(AiActivityLabels.subject(forOwnerId: "m1", in: msgs), "Hello")
        XCTAssertEqual(AiActivityLabels.subject(forOwnerId: "t:zz", in: msgs), "t:zz")
    }
}
