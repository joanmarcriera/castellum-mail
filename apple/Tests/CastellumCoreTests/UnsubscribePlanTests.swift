// Castellum - tests for the unsubscribe plan (FP39 redo, #3114 / #3046).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class UnsubscribePlanTests: XCTestCase {
    private func detail(mailto: String? = nil, url: String? = nil, oneClick: Bool = false) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "a:INBOX:1", accountId: "a", mailboxId: "a:INBOX",
                subject: "News", fromName: nil, fromAddress: "news@example.com",
                sentAt: nil, snippet: nil, hasAttachments: false, hasBody: nil,
                isRead: false, isStarred: false, threadCount: 1),
            rfc822MessageId: nil, addresses: [], textBody: nil, sanitizedHtml: nil,
            listUnsubscribeMailto: mailto, listUnsubscribeUrl: url, listUnsubscribeOneClick: oneClick)
    }

    func testOneClickHttpsShowsTheExactUrlAndHost() {
        let plan = UnsubscribePlan.make(for: detail(url: "https://Lists.Example.com/u/abc?x=1", oneClick: true))
        XCTAssertEqual(plan, .oneClick(url: "https://Lists.Example.com/u/abc?x=1", host: "lists.example.com"))
    }

    /// #3046: an https-only list (no one-click) has no in-app action, so it is not offered.
    func testHttpsOnlyWithoutOneClickIsNotOfferedAndTheSheetAgrees() {
        let d = detail(url: "https://example.com/unsub", oneClick: false)
        guard case .refused = UnsubscribePlan.make(for: d) else { return XCTFail("must be refused") }
        XCTAssertFalse(d.hasUnsubscribeAffordance)
    }

    func testDisguisedOrUnsafeLinksAreRefusedAndNotOffered() {
        let bad = [
            "http://example.com/u",                       // not https
            "https://example.com@evil.test/u",            // user-info disguise
            "https://user:pw@example.com/u",
            "https://xn--exmple-cua.com/u",               // punycode look-alike
            "https://exämple.com/u",                      // non-ASCII host
            "https://127.0.0.1/u",                        // IP literal
            "https://localhost/u",
            "https://intranet/u",                         // no dot
            "https://example.com/u v",                    // whitespace
            "ftp://example.com/u",
        ]
        for url in bad {
            let d = detail(url: url, oneClick: true)
            guard case .refused = UnsubscribePlan.make(for: d) else { return XCTFail("\(url) must be refused") }
            XCTAssertFalse(d.hasUnsubscribeAffordance, url)
        }
    }

    func testMailtoShowsAddressSubjectAndBodyExactly() {
        let plan = UnsubscribePlan.make(for: detail(mailto: "mailto:unsub@example.com?subject=Please%20remove%20me&body=Thanks"))
        XCTAssertEqual(plan, .mailto(address: "unsub@example.com", subject: "Please remove me", body: "Thanks"))
        XCTAssertEqual(UnsubscribePlan.make(for: detail(mailto: "mailto:unsub@example.com")),
                       .mailto(address: "unsub@example.com", subject: "unsubscribe", body: ""))
    }

    func testMailtoWithExtraRecipientsOrJunkIsRefused() {
        for m in ["mailto:a@example.com,b@example.com", "mailto:a@example.com?cc=b@example.com",
                  "mailto:a@example.com?bcc=b@example.com", "mailto:nobody", "mailto:a b@example.com"] {
            guard case .refused = UnsubscribePlan.make(for: detail(mailto: m)) else { return XCTFail("\(m) must be refused") }
        }
    }

    func testAnUnusableUrlFallsBackToAGoodMailto() {
        let plan = UnsubscribePlan.make(for: detail(mailto: "mailto:u@example.com", url: "http://example.com/u"))
        XCTAssertEqual(plan, .mailto(address: "u@example.com", subject: "unsubscribe", body: ""))
    }

    func testNothingUsableSaysSo() {
        XCTAssertEqual(UnsubscribePlan.make(for: detail()), .refused(reason: "This message has no recognised unsubscribe link."))
    }
}
